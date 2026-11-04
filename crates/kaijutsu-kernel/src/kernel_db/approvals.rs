//! Asks, continuations, and approval pairs.

use super::*;

/// One explicit model-drive epoch that may receive automatic continuation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContinuationOrigin {
    pub epoch: i64,
    pub opened_at: i64,
}

/// Record the model's ToolCall block that raised `request`, in the
/// transaction that creates the ask. The call must belong to the ask's context.
pub(crate) fn record_approval_tool_call(
    conn: &Connection, request: &str, context: ContextId, call: &BlockId,
) -> KernelDbResult<()> {
    if call.context_id != context {
        return Err(KernelDbError::Validation(format!(
            "tool call {} belongs to context {}, not the ask's context {context}", call.to_key(), call.context_id)));
    }
    conn.execute("INSERT INTO approval_tool_calls(request_id, block_id) VALUES (?1, ?2)",
        rusqlite::params![request, call.to_key()])?;
    Ok(())
}

/// Record the held drift block a drift ask answers for, in the ask's
/// transaction.
pub(crate) fn record_approval_drift_block(conn: &Connection, request: &str, block: &BlockId) -> KernelDbResult<()> {
    conn.execute("INSERT INTO approval_drift_blocks(request_id, block_id) VALUES (?1, ?2)",
        rusqlite::params![request, block.to_key()])?;
    Ok(())
}

/// What ending a turn's hold did to its ask (`KernelDb::end_held_ask`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeldAskEnd {
    /// Unanswered: abandoned.
    Abandoned,
    /// Answered but not run: spent without running.
    Spent,
    /// The approval worker already claimed it and settles the pair.
    Claimed,
}

impl KernelDb {
    /// Abandon every ask that has not reached a terminal state, once at
    /// kernel cold start. Returns how many rows moved.
    ///
    /// The public face of `approval_ledger::decide::abandon_unresolved_on_restart`
    /// for callers outside this crate: the ledger's whole API borrows a
    /// `Connection`, and [`Self::conn_for_ledger`] is `pub(crate)` on
    /// purpose, so the server crate reaches the sweep through here rather
    /// than through the raw connection.
    ///
    /// `reason` is displayed by `kj ledger show`, so it should name the
    /// condition without claiming execution never occurred: result reviews
    /// follow execution. See `docs/gate-resume.md`.
    pub fn abandon_unresolved_asks_on_restart(&self, reason: &str) -> KernelDbResult<usize> {
        Ok(approval_ledger::decide::abandon_unresolved_on_restart(
            self.conn_for_ledger(),
            reason,
        )?)
    }

    /// Link the ask's original pair and retain its execution receipt together.
    /// Executable asks reuse an existing receipt or adopt the supplied pair.
    /// The caller validates the pair before linking; context and performer
    /// must match the ask. A different pair or owner cannot replace a link.
    ///
    /// Joins an existing transaction; its caller must roll back on error.
    /// Turn pairs need a model notification after settlement; Session pairs
    /// are read directly by their connected player.
    pub fn link_ask_blocks(
        &self,
        request_id: &str,
        command_block_id: &BlockId,
        output_block_id: &BlockId,
        owner: approval_ledger::types::PairOwner,
    ) -> KernelDbResult<()> {
        if self.conn.is_autocommit() {
            return self.in_transaction(|db| db.link_ask_blocks(request_id, command_block_id, output_block_id, owner));
        }
        crate::shell_operations::link_approval_pair(self, request_id, command_block_id, output_block_id, owner)
    }

    /// The free-variable env snapshot recorded on this ask
    /// (`kj::env_snapshot::free_variable_values`, captured at
    /// `kj::gate::build_ask` time), in the order it was captured. Empty for
    /// an ask with no free variables, an unknown ask, or an origin that
    /// records no snapshot. `docs/gate-resume.md`.
    pub fn ask_env(&self, request_id: &str) -> KernelDbResult<Vec<approval_ledger::types::AskEnvRow>> {
        Ok(approval_ledger::ask::load_ask_env(self.conn_for_ledger(), request_id)?)
    }

    /// Open a new continuation epoch for an explicit model drive.
    pub fn begin_continuation(&self, context_id: ContextId, opened_at: i64) -> KernelDbResult<ContinuationOrigin> {
        if self.conn.is_autocommit() {
            return self.in_transaction(|db| db.begin_continuation(context_id, opened_at));
        }
        let previous: Option<i64> = self.conn.query_row(
            "SELECT epoch FROM context_continuations WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
            |row| row.get(0),
        ).optional()?;
        let epoch = previous.unwrap_or(0).checked_add(1)
            .ok_or_else(|| KernelDbError::Validation("continuation epoch overflow".into()))?;
        self.conn.execute(
            "INSERT INTO context_continuations (context_id, epoch, opened_at, last_request_at, yielded_at, signed_off_at, interrupted_at, interrupted_by)
             VALUES (?1, ?2, ?3, NULL, NULL, NULL, NULL, NULL)
             ON CONFLICT(context_id) DO UPDATE SET epoch = excluded.epoch, opened_at = excluded.opened_at,
                 last_request_at = NULL, yielded_at = NULL, signed_off_at = NULL, interrupted_at = NULL, interrupted_by = NULL",
            params![blob_param(context_id.as_bytes()), epoch, opened_at],
        )?;
        Ok(ContinuationOrigin { epoch, opened_at })
    }

    /// Stamp an actual provider inference request for this open epoch.
    pub fn record_continuation_request(&self, context_id: ContextId, epoch: i64, requested_at: i64) -> KernelDbResult<bool> {
        if self.conn.is_autocommit() {
            return self.in_transaction(|db| db.record_continuation_request(context_id, epoch, requested_at));
        }
        Ok(self.conn.execute(
            "UPDATE context_continuations SET last_request_at = ?1
             WHERE context_id = ?2 AND epoch = ?3 AND signed_off_at IS NULL",
            params![requested_at, blob_param(context_id.as_bytes()), epoch],
        )? != 0)
    }

    /// Admit inference from an accepted turn unless its performer was reassigned.
    /// Signoff and newer explicit drives only close automatic resumption; an
    /// accepted request can finish without refreshing a closed or newer window.
    pub(crate) fn admit_inference_request(&self, context_id: ContextId, epoch: i64, requested_at: i64) -> KernelDbResult<bool> {
        if self.conn.is_autocommit() {
            return self.in_transaction(|db| db.admit_inference_request(context_id, epoch, requested_at));
        }
        let valid: bool = self.conn.query_row(
            "SELECT ?2 > invalidated_through AND ?2 <= epoch FROM context_continuations WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes()), epoch], |row| row.get(0),
        )?;
        if !valid { return Ok(false); }
        self.record_continuation_request(context_id, epoch, requested_at)?;
        Ok(true)
    }

    /// Return the current unsigned continuation epoch, if this context has one.
    pub fn continuation_epoch(&self, context_id: ContextId) -> KernelDbResult<Option<i64>> {
        self.conn.query_row(
            "SELECT epoch FROM context_continuations WHERE context_id = ?1 AND signed_off_at IS NULL",
            params![blob_param(context_id.as_bytes())],
            |row| row.get(0),
        ).optional().map_err(Into::into)
    }

    /// Mark the matching epoch as yielded without extending its window.
    pub fn record_continuation_yield(&self, context_id: ContextId, epoch: i64, yielded_at: i64) -> KernelDbResult<bool> {
        if self.conn.is_autocommit() {
            return self.in_transaction(|db| db.record_continuation_yield(context_id, epoch, yielded_at));
        }
        Ok(self.conn.execute(
            "UPDATE context_continuations SET yielded_at = ?1
             WHERE context_id = ?2 AND epoch = ?3 AND signed_off_at IS NULL",
            params![yielded_at, blob_param(context_id.as_bytes()), epoch],
        )? != 0)
    }

    /// End the current continuation epoch through an explicit signoff.
    pub fn sign_off_continuation(&self, context_id: ContextId, signed_off_at: i64) -> KernelDbResult<bool> {
        if self.conn.is_autocommit() {
            return self.in_transaction(|db| db.sign_off_continuation(context_id, signed_off_at));
        }
        Ok(self.conn.execute(
            "UPDATE context_continuations SET signed_off_at = ?1
             WHERE context_id = ?2 AND signed_off_at IS NULL",
            params![signed_off_at, blob_param(context_id.as_bytes())],
        )? != 0)
    }

    /// Record that this context's open continuation epoch was interrupted
    /// (`kj interrupt` or RPC `interruptContext`), closing it to automatic
    /// resume. Distinct from [`Self::sign_off_continuation`]: that is the
    /// performer's own handoff close, this is a reviewer or director
    /// stopping the loop from outside. Idempotent and DB-first — a caller
    /// records this before touching the running turn, so a completion
    /// racing the interrupt can never claim a resume. Returns `false`
    /// without writing when there is no open epoch (none opened, already
    /// signed off) or one is already interrupted.
    pub fn record_continuation_interrupt(
        &self,
        context_id: ContextId,
        interrupted_by: PrincipalId,
        interrupted_at: i64,
    ) -> KernelDbResult<bool> {
        if self.conn.is_autocommit() {
            return self.in_transaction(|db| db.record_continuation_interrupt(context_id, interrupted_by, interrupted_at));
        }
        Ok(self.conn.execute(
            "UPDATE context_continuations SET interrupted_at = ?1, interrupted_by = ?2
             WHERE context_id = ?3 AND signed_off_at IS NULL AND interrupted_at IS NULL",
            params![interrupted_at, blob_param(interrupted_by.as_bytes()), blob_param(context_id.as_bytes())],
        )? != 0)
    }

    /// Who interrupted this context's open continuation epoch, and when —
    /// `None` when it was never interrupted (or has no continuation row).
    pub fn continuation_interrupt(&self, context_id: ContextId) -> KernelDbResult<Option<(i64, PrincipalId)>> {
        self.conn.query_row(
            "SELECT interrupted_at, interrupted_by FROM context_continuations
             WHERE context_id = ?1 AND interrupted_at IS NOT NULL",
            params![blob_param(context_id.as_bytes())],
            |row| Ok((row.get(0)?, read_principal_id(row, 1)?)),
        ).optional().map_err(Into::into)
    }

    /// Whether a settled operation may automatically request another turn.
    /// The epoch must have yielded, but the deadline is its last provider
    /// inference request rather than the yield itself. An interrupted epoch
    /// (`interrupted_at` set) never resumes automatically, even within the
    /// window — only an explicit new epoch (`begin_continuation`) reopens it.
    pub fn automatic_resume_allowed(
        &self,
        context_id: ContextId,
        epoch: i64,
        now: i64,
        window_ms: i64,
    ) -> KernelDbResult<bool> {
        let row: Option<(i64, Option<i64>, Option<i64>, Option<i64>, Option<i64>)> = self.conn.query_row(
            "SELECT epoch, last_request_at, yielded_at, signed_off_at, interrupted_at FROM context_continuations WHERE context_id = ?1",
            params![blob_param(context_id.as_bytes())],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        ).optional()?;
        let Some((current, Some(last_request_at), Some(_), None, None)) = row else { return Ok(false) };
        Ok(current == epoch && now.saturating_sub(last_request_at) <= window_ms)
    }

    /// Claim one automatic resume from an eligible yielded continuation.
    pub fn claim_automatic_resume(&self, context_id: ContextId, epoch: i64, now: i64, window_ms: i64) -> KernelDbResult<bool> {
        if self.conn.is_autocommit() {
            return self.in_transaction(|db| db.claim_automatic_resume(context_id, epoch, now, window_ms));
        }
        if !self.automatic_resume_allowed(context_id, epoch, now, window_ms)? {
            return Ok(false);
        }
        self.conn.execute(
            "UPDATE context_continuations SET yielded_at = NULL WHERE context_id = ?1 AND epoch = ?2",
            params![blob_param(context_id.as_bytes()), epoch],
        )?;
        Ok(true)
    }

    /// The public face of `approval_ledger::ask::list_pending` — the asks
    /// waiting on a human right now.
    ///
    /// `claimed` rows are excluded, matching the queue `kj ledger list`
    /// shows: an answerer is already working them.
    pub fn list_pending_asks(&self) -> KernelDbResult<Vec<approval_ledger::types::ApprovalRow>> {
        Ok(approval_ledger::ask::list_pending(self.conn_for_ledger())?)
    }

    /// Every ask that is not yet resolved: `pending` and `claimed`. A caller
    /// that asks whether anything is still open in a context reads this;
    /// [`list_pending_asks`](Self::list_pending_asks) hides a claimed row.
    pub fn list_unresolved_asks(&self) -> KernelDbResult<Vec<approval_ledger::types::ApprovalRow>> {
        Ok(approval_ledger::ask::list_unresolved(self.conn_for_ledger())?)
    }

    /// Uncollected human answers for the execution/resume driver.
    /// Result reviews are consumed by their retained outcome owner.
    ///
    /// Runtime delivery reads through this API; the ledger owns its schema.
    /// See `docs/gate-resume.md`.
    pub fn undelivered_answers(
        &self,
    ) -> KernelDbResult<Vec<approval_ledger::ask::UndeliveredAnswer>> {
        Ok(approval_ledger::ask::undelivered_answers(
            self.conn_for_ledger(),
        )?)
    }

    /// Record the caller's publication contract before an ask becomes visible.
    #[cfg(test)]
    pub(crate) fn create_approval_ask(&self, ask: &approval_ledger::types::NewAsk, publishes_pair: bool) -> KernelDbResult<String> {
        self.create_approval_ask_recorded(ask, publishes_pair, |_, _| Ok(()))
    }

    /// Commit related caller state before publishing an ask. The callback must
    /// use this connection and must not acquire the database guard again.
    pub(crate) fn create_approval_ask_recorded(
        &self, ask: &approval_ledger::types::NewAsk, publishes_pair: bool,
        record: impl FnOnce(&rusqlite::Connection, &str) -> KernelDbResult<()>,
    ) -> KernelDbResult<String> {
        approval_ledger::ask::create_ask_recorded(self.conn_for_ledger(), ask, |conn, request| {
            if publishes_pair {
                conn.execute("INSERT INTO approval_pair_handoffs(request_id) VALUES (?1)", [request])?;
            }
            record(conn, request)
        })
    }

    /// The model's ToolCall block recorded with this ask, if a model call raised it.
    pub(crate) fn approval_tool_call(&self, request: &str) -> KernelDbResult<Option<BlockId>> {
        let key: Option<String> = self.conn.query_row("SELECT block_id FROM approval_tool_calls WHERE request_id=?1",
            [request], |row| row.get(0)).optional()?;
        key.map(|key| BlockId::from_key(&key)
            .ok_or_else(|| KernelDbError::Validation(format!("ask {request} records a malformed tool call block {key}"))))
            .transpose()
    }

    /// The drift ask recorded with a held drift block.
    pub(crate) fn approval_for_drift_block(&self, block: &BlockId) -> KernelDbResult<Option<String>> {
        Ok(self.conn.query_row("SELECT request_id FROM approval_drift_blocks WHERE block_id=?1",
            [block.to_key()], |row| row.get(0)).optional()?)
    }

    /// The held drift block recorded with a drift ask.
    pub(crate) fn approval_drift_block(&self, request: &str) -> KernelDbResult<Option<BlockId>> {
        let key: Option<String> = self.conn.query_row("SELECT block_id FROM approval_drift_blocks WHERE request_id=?1",
            [request], |row| row.get(0)).optional()?;
        key.map(|key| BlockId::from_key(&key)
            .ok_or_else(|| KernelDbError::Validation(format!("ask {request} records a malformed drift block {key}"))))
            .transpose()
    }

    pub(crate) fn approval_pair_expected(&self, request: &str) -> KernelDbResult<bool> {
        Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM approval_pair_handoffs WHERE request_id=?1)",
            [request], |row| row.get(0))?)
    }

    /// A caller that promised a pair releases delivery with its Waiting result.
    /// Call with the same database guard used to admit approval execution.
    pub(crate) fn approval_pair_ready(&self, request: &str) -> KernelDbResult<bool> {
        Ok(self.conn.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM approval_pair_handoffs WHERE request_id=?1)
             OR EXISTS(SELECT 1 FROM approval_pair_handoffs h JOIN approvals a USING(request_id)
                 WHERE h.request_id=?1 AND h.released=1 AND h.abandoned_reason IS NULL AND a.command_block_id IS NOT NULL
                 AND a.output_block_id IS NOT NULL AND a.pair_owner IS NOT NULL)",
            [request], |row| row.get(0),
        )?)
    }

    /// Release a paired caller inside the Waiting result's journal transaction.
    pub(crate) fn release_approval_pair(&self, request: &str) -> KernelDbResult<()> {
        if !self.approval_pair_expected(request)? { return Ok(()); }
        if self.conn.is_autocommit() { return Err(KernelDbError::Validation("approval handoff requires the result transaction".into())); }
        let changed = self.conn.execute(
            "UPDATE approval_pair_handoffs SET released=1 WHERE request_id=?1 AND abandoned_reason IS NULL
             AND EXISTS(SELECT 1 FROM approvals WHERE request_id=?1 AND command_block_id IS NOT NULL
                 AND output_block_id IS NOT NULL AND pair_owner IS NOT NULL)", [request],
        )?;
        if changed != 1 { return Err(KernelDbError::Validation("approval handoff requires a complete pair link".into())); }
        Ok(())
    }

    pub(crate) fn approval_pair_abandoned_reason(&self, request: &str) -> KernelDbResult<Option<String>> {
        Ok(self.conn.query_row("SELECT abandoned_reason FROM approval_pair_handoffs WHERE request_id=?1",
            [request], |row| row.get(0)).optional()?.flatten())
    }

    /// Retire an unpublished invocation without replacing its reviewer's answer.
    /// Joins result acceptance; errors require rollback by the caller.
    pub(crate) fn abandon_approval_pair(&self, request: &str, pair: Option<(&BlockId, &BlockId)>, reason: &str) -> KernelDbResult<()> {
        if self.conn.is_autocommit() { return Err(KernelDbError::Validation("approval abandonment requires a transaction".into())); }
        let changed = self.conn.execute(
            "UPDATE approval_pair_handoffs SET abandoned_reason=?2
             WHERE request_id=?1 AND released=0 AND abandoned_reason IS NULL", rusqlite::params![request, reason])?;
        if changed == 0 { return Ok(()); }
        let row = self.get_approval(request)?.ok_or_else(|| approval_ledger::error::LedgerError::NotFound(request.into()))?;
        if let Some((command, output)) = pair {
            if row.context_id != command.context_id.as_bytes() || output.context_id != command.context_id
                || row.actor_id.as_deref() != Some(command.principal_id.as_bytes().as_slice()) {
                return Err(KernelDbError::Validation("approval abandonment does not match its context and performer".into()));
            }
            match (row.command_block_id.as_deref(), row.output_block_id.as_deref(), row.pair_owner) {
                (None, None, None) => {}
                (Some(prior_command), Some(prior_output), Some(_))
                    if prior_command == command.to_key() && prior_output == output.to_key() => {}
                _ => return Err(KernelDbError::Validation("approval abandonment belongs to a different block pair".into())),
            }
        } else if row.command_block_id.is_some() || row.output_block_id.is_some() || row.pair_owner.is_some() {
            return Err(KernelDbError::Validation("linked approval abandonment requires its original pair".into()));
        }
        match row.status {
            crate::ApprovalStatus::Pending | crate::ApprovalStatus::Claimed => {
                approval_ledger::decide::abandon(self.conn_for_ledger(), request, Some(reason))?;
            }
            crate::ApprovalStatus::Allowed | crate::ApprovalStatus::Denied => { self.redeem_ask(request)?; }
            crate::ApprovalStatus::Abandoned if row.decided_option.as_deref() == Some("cancel") => { self.redeem_ask(request)?; }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn unpublished_approval_pairs(&self) -> KernelDbResult<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT request_id FROM approval_pair_handoffs WHERE released=0 AND abandoned_reason IS NULL ORDER BY request_id")?;
        Ok(stmt.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    /// One approval row, whole — the executable source, the cwd, the block
    /// pair and the decision, all of which the answer summary omits.
    ///
    /// The gate-resume driver reads this before acting on an answer:
    /// whether an ask executes on approval or its caller must retry is a
    /// property of the row, not of the summary.
    pub fn get_approval(
        &self,
        request_id: &str,
    ) -> KernelDbResult<Option<approval_ledger::types::ApprovalRow>> {
        Ok(approval_ledger::ask::get_approval(
            self.conn_for_ledger(),
            request_id,
        )?)
    }

    /// Consume an answered ask's single-use delivery. `Ok(true)` means THIS
    /// call inserted the redemption row and owns the action; `Ok(false)`
    /// means someone else already has it.
    ///
    /// The public face of `approval_ledger::decide::redeem_ask`, for the
    /// same reason the reads above are wrapped: exactly-once lives in the
    /// `approval_redemptions` primary key, and the server reaches it
    /// through the kernel rather than through the ledger connection.
    pub fn redeem_ask(&self, request_id: &str) -> KernelDbResult<bool> {
        Ok(approval_ledger::decide::redeem_ask(
            self.conn_for_ledger(),
            request_id,
        )?)
    }

    /// Whether an answer to this ask has been spent: delivered, claimed by
    /// the approval worker, or learned from.
    pub(crate) fn ask_redeemed(&self, request_id: &str) -> KernelDbResult<bool> {
        Ok(approval_ledger::ask::redeemed_at(self.conn_for_ledger(), request_id)?.is_some())
    }

    /// End a model turn's hold on its ask because the turn stopped waiting.
    /// An unanswered ask is abandoned with `reason`; an answer nobody has run
    /// is spent without running, so the approval worker never acts on it. Returns
    /// [`HeldAskEnd::Claimed`] when the worker already owns the answer: it
    /// is running or settling the command, and it settles the pair.
    pub(crate) fn end_held_ask(&self, request_id: &str, reason: &str) -> KernelDbResult<HeldAskEnd> {
        self.in_transaction(|db| {
            let conn = db.conn_for_ledger();
            let row = approval_ledger::ask::get_approval(conn, request_id)?
                .ok_or_else(|| KernelDbError::Validation(format!("held ask {request_id} has no row")))?;
            // An ordinary abandonment is terminal and never delivered, so it
            // needs no redemption.
            if matches!(row.status, crate::ApprovalStatus::Pending | crate::ApprovalStatus::Claimed) {
                approval_ledger::decide::abandon(conn, request_id, Some(reason))?;
                return Ok(HeldAskEnd::Abandoned);
            }
            Ok(if approval_ledger::decide::redeem_ask(conn, request_id)? { HeldAskEnd::Spent } else { HeldAskEnd::Claimed })
        })
    }
}

#[cfg(test)]
mod tests;
