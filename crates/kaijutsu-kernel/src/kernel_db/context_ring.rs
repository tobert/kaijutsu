//! Context state, archive, promote and demote, and rotation.

use super::*;

/// Hard cap on the number of contexts holding a ring-0 seat
/// (`promoted_at IS NOT NULL AND archived_at IS NULL`) at once. Ring 0
/// renders exactly this many slots, addressed by digit hotkeys 0-9, and is
/// Amy's hand-curated seat row: seats must never appear or vanish without an
/// explicit act, so [`KernelDb::promote_context`] refuses loudly instead of
/// bumping the oldest occupant when the ring is full. Derived from the
/// canonical [`kaijutsu_types::RING_SLOTS`] (the same 10 the app's ring
/// layout seats).
pub const ACTIVE_RING_CAPACITY: i64 = kaijutsu_types::RING_SLOTS as i64;

/// Outcome of one step on the demote ladder — see
/// [`KernelDb::demote_context`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemoteOutcome {
    /// Was promoted (ring 0) → `promoted_at` cleared, back to automatic placement.
    Unpromoted,
    /// Was neither promoted nor demoted → `demoted_at` set (pushed to the demoted ring).
    Demoted,
    /// Was already demoted → archived (one step further outward, past the rim).
    Archived,
}

/// Outcome of a promote — see [`KernelDb::promote_context`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromoteOutcome {
    /// Newly seated in ring 0 (`promoted_at` stamped, `demoted_at` cleared).
    Promoted,
    /// Already held a seat — no-op, original timestamp preserved.
    AlreadyPromoted,
    /// Was archived — resurrected: promote is the resurrection door (the
    /// archive is memory to drift back from, not trash). `archived_at`
    /// cleared, a FRESH `promoted_at` stamped (the old seat was surrendered
    /// at archive time), `demoted_at` cleared. `concluded` reports whether
    /// the row is still concluded — placement is orthogonal to conclusion
    /// ("concluded but seated for review" is a legit state), and callers
    /// need it to restore the right DriftRouter lifecycle state.
    Resurrected {
        /// The resurrected row's `concluded_at` is still set.
        concluded: bool,
    },
}

/// What an archive changed: whether the context was live, and the judge
/// shadows archived with it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Archived {
    /// The context was live before the archive.
    pub context: bool,
    /// The live judge shadows of the context that the same transaction
    /// archived, oldest first.
    pub shadows: Vec<ContextId>,
}

/// What [`KernelDb::commit_rotation`] changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RotationCommit {
    /// How many characters' `handoff_ctx` moved to the successor.
    pub handoffs_moved: usize,
    /// The predecessor's judge shadows, archived with it.
    pub shadows: Vec<ContextId>,
}

/// What [`KernelDb::retire_with_contexts`] changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Retirement {
    /// The contexts the character performed, concluded and archived.
    pub archived: Vec<ContextId>,
    /// Their judge shadows, archived with them.
    pub shadows: Vec<ContextId>,
}

impl KernelDb {
    /// Archive a context while retaining its history. Returns true if it was
    /// active. Its live judge shadows are archived with it, told by their
    /// judge's model alone ([`Self::is_judge_shadow`] with no gate.toml
    /// names); [`Self::archive_with_shadows`] takes the names and returns the
    /// shadows, for a caller that keeps the drift router in step.
    pub fn archive_context(&self, id: ContextId) -> KernelDbResult<bool> {
        Ok(self.archive_with_shadows(id, None)?.context)
    }

    /// Archive a context and its live judge shadows in one transaction, while
    /// retaining their history. Clears ring placement and abandons unresolved
    /// asks without deleting their records. Decided asks retain their
    /// answers. State and sweep commit together; an already archived row
    /// retries unresolved ask cleanup and still archives a live shadow. This
    /// method does not stop running work.
    ///
    /// `judges` are the judge characters the council blocks of a gate.toml
    /// that loaded name, or `None` when it did not load
    /// ([`Self::is_judge_shadow`]).
    pub fn archive_with_shadows(&self, id: ContextId, judges: Option<&[String]>) -> KernelDbResult<Archived> {
        if self.conn.is_autocommit() {
            return self.in_transaction(|db| db.archive_context_writes(id, judges));
        }
        self.conn.execute_batch("SAVEPOINT archive_context")?;
        let result = self.archive_context_writes(id, judges);
        match result {
            Ok(archived) => {
                self.conn.execute_batch("RELEASE SAVEPOINT archive_context")?;
                Ok(archived)
            }
            Err(error) => {
                self.conn.execute_batch(
                    "ROLLBACK TO SAVEPOINT archive_context; RELEASE SAVEPOINT archive_context",
                )?;
                Err(error)
            }
        }
    }

    /// Why `id` may not be archived yet: it holds drifts whose asks are
    /// unanswered or unsettled. A held drift is settled by its ask, and an
    /// archived context answers nothing, so archiving would leave the drift
    /// held with no ask that can release it. Verbs that archive check this
    /// before they run anything; [`Self::archive_context_writes`] checks it
    /// again inside the write.
    pub(crate) fn held_drift_refusal(&self, id: ContextId) -> KernelDbResult<Option<String>> {
        let held = approval_ledger::ask::list_unsettled_drift_asks(&self.conn, id.as_bytes())?;
        if held.is_empty() {
            return Ok(None);
        }
        let asks = held.iter().map(|id| id.split('-').next().unwrap_or(id)).collect::<Vec<_>>().join(", ");
        Ok(Some(format!(
            "context {} holds {} drift(s) waiting for its human (ask {asks}); answer each with \
             `kj ledger allow <id>` or `kj ledger deny <id>`, then run the verb again",
            id.short(), held.len()
        )))
    }

    /// Why `id` may not be archived by any path, whoever asks: it is a root
    /// console, which its character would be left without, or an MCP hub
    /// that live sessions have as their parent. Verbs check this before
    /// they run anything, for their message; [`Self::archive_context_writes`]
    /// checks it again inside the write. A rotation and a retirement move or
    /// clear `root_ctx` first, in the transaction that archives.
    pub(crate) fn archive_refusal(&self, id: ContextId) -> KernelDbResult<Option<String>> {
        let label = self.get_context(id)?.and_then(|row| row.label).unwrap_or_else(|| id.short());
        if self.is_root_console(id)? {
            return Ok(Some(format!(
                "{label} is a root console, and archiving it would leave its character with none; \
                 `kj context rotate {label}` replaces it"
            )));
        }
        let sessions = self.live_mcp_sessions(id)?;
        if !sessions.is_empty() {
            let names = sessions.iter().map(|id| self.context_name(*id)).collect::<Vec<_>>().join(", ");
            return Ok(Some(format!(
                "{label} is an MCP hub, and its live sessions {names} have it as their parent; \
                 end them first (`kj context archive <session> --confirm`)"
            )));
        }
        Ok(None)
    }

    /// Whether `row` is a judge shadow: its parent is the context it answers
    /// to, and its performer is a judge. A character whose model is on a
    /// council server (a backend of kind `mk`) is a judge, since a council
    /// server is never a chat model; so is one some council block names as
    /// `judge`, in `judges`. `judges` is `None` when gate.toml did not load,
    /// and then the model alone tells.
    pub fn is_judge_shadow(&self, row: &ContextRow, judges: Option<&[String]>) -> KernelDbResult<bool> {
        let (Some(performer), Some(parent)) = (row.performer, row.forked_from) else { return Ok(false) };
        if row.senpai_ctx != Some(parent) {
            return Ok(false);
        }
        if let Some(model) = self.get_character_model(performer)?
            && self.backend_is_council_server(model.backend_id)?
        {
            return Ok(true);
        }
        let Some(judges) = judges else { return Ok(false) };
        Ok(self.get_character(performer)?.is_some_and(|sheet| judges.contains(&sheet.name)))
    }

    pub(super) fn archive_context_writes(&self, id: ContextId, judges: Option<&[String]>) -> KernelDbResult<Archived> {
        let context = self.archive_one_context(id)?;
        let mut shadows = Vec::new();
        for child in self.fork_children(id)? {
            if self.is_judge_shadow(&child, judges)? && self.archive_one_context(child.context_id)? {
                shadows.push(child.context_id);
            }
        }
        if !shadows.is_empty() {
            info!(context = %id.short(), shadows = shadows.len(), "archived the context's judge shadows with it");
        }
        Ok(Archived { context, shadows })
    }

    /// The archive of one context, with no shadows: the state, the ring
    /// placement, the ask sweep, and the `console_ctx` and `mcp_hub_ctx`
    /// pointers that name it, which are cleared. A context that
    /// [`Self::archive_refusal`] or [`Self::held_drift_refusal`] refuses
    /// stays live.
    fn archive_one_context(&self, id: ContextId) -> KernelDbResult<bool> {
        if let Some(why) = self.archive_refusal(id)? {
            return Err(KernelDbError::Validation(why));
        }
        if let Some(why) = self.held_drift_refusal(id)? {
            return Err(KernelDbError::Validation(why));
        }
        let now = now_millis();
        let updated = self.conn.execute(
            "UPDATE contexts
                SET archived_at = ?1, context_state = 'archived',
                    promoted_at = NULL, demoted_at = NULL
             WHERE context_id = ?2 AND archived_at IS NULL",
            params![now, blob_param(id.as_bytes())],
        )?;
        let swept = approval_ledger::decide::abandon_unresolved_for_archived_context(
            &self.conn,
            id.as_bytes(),
            "the context that raised this was archived; this ask cannot authorize further work",
        )?;
        if swept > 0 {
            info!(context = %id.short(), swept, "archived context: swept unresolved asks");
        }
        let consoles = self.conn.execute(
            "UPDATE characters SET console_ctx = NULL WHERE console_ctx = ?1",
            params![blob_param(id.as_bytes())],
        )?;
        let hubs = self.conn.execute(
            "UPDATE characters SET mcp_hub_ctx = NULL WHERE mcp_hub_ctx = ?1",
            params![blob_param(id.as_bytes())],
        )?;
        if consoles + hubs > 0 {
            info!(context = %id.short(), consoles, hubs, "archived context: cleared the character pointers that named it");
        }
        Ok(updated > 0)
    }

    /// Finish a rotation in one transaction: archive `predecessor` and its
    /// judge shadows ([`Self::archive_with_shadows`], with `judges`), give
    /// `successor` its label and ring placement, and point a character's
    /// `handoff_ctx` at the successor when it named the predecessor. A
    /// predecessor that is missing or already archived refuses. Moves a
    /// character's `root_ctx` and `console_ctx` the same way; the archived
    /// root console then takes the successor as its senpai. Both pointers
    /// move before the archive, which would otherwise refuse a root console
    /// and clear the console pointer. The
    /// kernel moves no other context that answers to the predecessor.
    pub fn commit_rotation(
        &self,
        predecessor: ContextId,
        successor: ContextId,
        judges: Option<&[String]>,
    ) -> KernelDbResult<RotationCommit> {
        self.in_transaction(|db| {
            let row = db.get_context(predecessor)?.ok_or_else(|| {
                KernelDbError::NotFound(format!("context {}", predecessor.short()))
            })?;
            // A console pointer moving does not touch the predecessor's senpai.
            db.conn.execute(
                "UPDATE characters SET console_ctx = ?1 WHERE console_ctx = ?2",
                params![blob_param(successor.as_bytes()), blob_param(predecessor.as_bytes())],
            )?;
            let root_ctx_moved = db.conn.execute(
                "UPDATE characters SET root_ctx = ?1 WHERE root_ctx = ?2",
                params![blob_param(successor.as_bytes()), blob_param(predecessor.as_bytes())],
            )?;
            let archived = db.archive_with_shadows(predecessor, judges)?;
            if !archived.context {
                return Err(KernelDbError::Validation(format!(
                    "context {} was archived before the rotation finished",
                    predecessor.short()
                )));
            }
            db.conn.execute(
                "UPDATE contexts SET label = ?1, promoted_at = ?2, demoted_at = ?3 WHERE context_id = ?4",
                params![row.label, row.promoted_at, row.demoted_at, blob_param(successor.as_bytes())],
            )?;
            let moved = db.conn.execute(
                "UPDATE characters SET handoff_ctx = ?1 WHERE handoff_ctx = ?2",
                params![blob_param(successor.as_bytes()), blob_param(predecessor.as_bytes())],
            )?;
            if root_ctx_moved > 0 {
                // The archived console answers to its successor, so it can be
                // promoted or moved again.
                db.conn.execute(
                    "UPDATE contexts SET senpai_ctx = ?1 WHERE context_id = ?2 AND senpai_ctx IS NULL",
                    params![blob_param(successor.as_bytes()), blob_param(predecessor.as_bytes())],
                )?;
            }
            Ok(RotationCommit { handoffs_moved: moved, shadows: archived.shadows })
        })
    }

    /// Retire `principal_id` in one transaction: stamp `retired_at`, clear
    /// its `root_ctx`, `console_ctx` and `mcp_hub_ctx`, then conclude and
    /// archive each live context it performs, with that context's judge
    /// shadows ([`Self::archive_with_shadows`], with `judges`). For a root
    /// character, any `root_ctx` naming a context it performs clears first,
    /// so its root consoles are archived with it. Any refusal or fault rolls
    /// the whole retirement back. `None` when the character was already
    /// retired; an unknown character refuses.
    pub fn retire_with_contexts(
        &self,
        principal_id: PrincipalId,
        at: i64,
        judges: Option<&[String]>,
    ) -> KernelDbResult<Option<Retirement>> {
        self.in_transaction(|db| {
            let sheet = db.get_character(principal_id)?
                .ok_or_else(|| KernelDbError::NotFound(format!("character {}", principal_id.short())))?;
            if sheet.retired_at.is_some() {
                return Ok(None);
            }
            db.retire_character(principal_id, at)?;
            db.set_character_root_ctx(principal_id, None)?;
            db.set_character_console_ctx(principal_id, None)?;
            db.set_character_mcp_hub_ctx(principal_id, None)?;
            if sheet.root {
                db.conn.execute(
                    "UPDATE characters SET root_ctx = NULL
                     WHERE root_ctx IN (SELECT context_id FROM contexts WHERE performer = ?1 AND archived_at IS NULL)",
                    params![blob_param(principal_id.as_bytes())],
                )?;
            }
            let mut retirement = Retirement::default();
            for ctx in db.contexts_played_by(principal_id)? {
                db.conclude_context(ctx.context_id)?;
                let archived = db.archive_with_shadows(ctx.context_id, judges)?;
                retirement.archived.push(ctx.context_id);
                retirement.shadows.extend(archived.shadows);
            }
            Ok(Some(retirement))
        })
    }

    /// Update the lifecycle state of a context.
    pub fn update_context_state(
        &self,
        id: ContextId,
        state: ContextState,
    ) -> KernelDbResult<bool> {
        let updated = self.conn.execute(
            "UPDATE contexts SET context_state = ?1 WHERE context_id = ?2",
            params![state.as_str(), blob_param(id.as_bytes())],
        )?;
        Ok(updated > 0)
    }

    /// Conclude a context: set `context_state = 'concluded'` and stamp
    /// `concluded_at` (first time only — idempotent re-conclude keeps the
    /// original timestamp). Only an active (non-archived) context can be
    /// concluded. Returns `true` if a row was newly concluded, `false` if the
    /// context was unknown, archived, or already concluded. Also clears
    /// `promoted_at` — concluding is the mux-exit that gives up the ring-0
    /// seat — but deliberately preserves `demoted_at`: explicit demotion is
    /// sticky through conclusion, so a demoted context stays in the demoted
    /// ring instead of floating back to the auto pool.
    pub fn conclude_context(&self, id: ContextId) -> KernelDbResult<bool> {
        let now = now_millis();
        let updated = self.conn.execute(
            "UPDATE contexts
                SET context_state = 'concluded', concluded_at = ?1, promoted_at = NULL
             WHERE context_id = ?2
               AND archived_at IS NULL
               AND concluded_at IS NULL",
            params![now, blob_param(id.as_bytes())],
        )?;
        Ok(updated > 0)
    }

    /// Promote a context into ring 0 ("active"). First-write-wins on
    /// `promoted_at` — re-promoting an already-promoted context is a no-op
    /// that preserves the original timestamp, so ring-0 order stays
    /// append-stable — and clears `demoted_at`. Promoting an ARCHIVED
    /// context resurrects it: `archived_at` cleared, a fresh `promoted_at`
    /// stamped (the old seat was surrendered at archive time), `concluded_at`
    /// untouched — see [`PromoteOutcome::Resurrected`]. Enforces
    /// [`ACTIVE_RING_CAPACITY`] on every seat grant (resurrection included):
    /// a full ring fails loudly rather than silently bumping anything —
    /// ring-0 is a hand-curated row of seats addressed by digit hotkeys, so
    /// seats must never appear or vanish without an explicit act. On the
    /// ring-full error an archived context STAYS archived (no partial write).
    pub fn promote_context(&self, id: ContextId) -> KernelDbResult<PromoteOutcome> {
        let (promoted_at, archived_at, concluded_at): (Option<i64>, Option<i64>, Option<i64>) =
            self.conn
                .query_row(
                    "SELECT promoted_at, archived_at, concluded_at
                     FROM contexts WHERE context_id = ?1",
                    params![blob_param(id.as_bytes())],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?
                .ok_or_else(|| KernelDbError::NotFound(format!("context {}", id.short())))?;

        if archived_at.is_none() && promoted_at.is_some() {
            return Ok(PromoteOutcome::AlreadyPromoted);
        }

        let active_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM contexts WHERE promoted_at IS NOT NULL AND archived_at IS NULL",
            [],
            |row| row.get(0),
        )?;
        if active_count >= ACTIVE_RING_CAPACITY {
            return Err(KernelDbError::Validation(format!(
                "active ring full ({ACTIVE_RING_CAPACITY} seats) — demote something first"
            )));
        }

        if archived_at.is_some() {
            if self.senpai_of(id)?.is_none() && !self.is_root_console(id)? {
                return Err(KernelDbError::Validation(format!(
                    "context {} has no senpai, and a live context answers to one; \
                     give it one with `kj context set <ctx> --senpai <ctx>` before promoting it",
                    self.context_name(id)
                )));
            }
            // Resurrection: unarchive + a FRESH promoted_at in one write.
            // `archive_context` clears promoted_at, so an archived row
            // shouldn't still carry one — but stamp unconditionally anyway:
            // the old seat was surrendered at archive time either way.
            self.conn.execute(
                // The restored state is derived from `concluded_at`, not
                // remembered from before the archive: the timestamps are what
                // survive, so they are what the state is read back out of.
                "UPDATE contexts
                    SET archived_at = NULL,
                        context_state = CASE WHEN concluded_at IS NULL
                                             THEN 'live' ELSE 'concluded' END,
                        promoted_at = ?1, demoted_at = NULL
                 WHERE context_id = ?2",
                params![now_millis(), blob_param(id.as_bytes())],
            )?;
            return Ok(PromoteOutcome::Resurrected {
                concluded: concluded_at.is_some(),
            });
        }

        self.conn.execute(
            "UPDATE contexts SET promoted_at = ?1, demoted_at = NULL
             WHERE context_id = ?2 AND promoted_at IS NULL",
            params![now_millis(), blob_param(id.as_bytes())],
        )?;
        Ok(PromoteOutcome::Promoted)
    }

    /// Push a context outward one step on the demote ladder (kernel-owned
    /// policy, one place): already demoted → archives it (reuses
    /// [`Self::archive_context`]'s write — the context and its judge shadows,
    /// no other subtree recursion, no latch); promoted → clears `promoted_at` (back to
    /// automatic placement); neither → sets `demoted_at`. Each call is
    /// exactly one step outward. Errors if the context is unknown or already
    /// archived (nothing further out to step to).
    ///
    /// `demoted_at` is checked FIRST. On every legal state the order is
    /// irrelevant (the kernel keeps the two stamps mutually exclusive), but
    /// on a corrupt both-stamps row it makes this ladder agree with the app
    /// layout's demoted-wins rule (`kaijutsu_viz::layout::assign_ring_seats`)
    /// instead of no-op-looping through unpromote while the card sits in
    /// ring 3.
    pub fn demote_context(&self, id: ContextId) -> KernelDbResult<DemoteOutcome> {
        let archives = self.get_context(id)?.is_some_and(|row| row.demoted_at.is_some());
        Ok(self.demote_context_with_shadows(id, None, archives)?.0)
    }

    /// [`Self::demote_context`], whose archive step archives the context's
    /// judge shadows with it ([`Self::archive_with_shadows`], with `judges`)
    /// and returns them. `archives` is the step the caller decided on and
    /// prepared for: true for the archive step, whose checks and archive
    /// lifecycle the caller ran. A row whose next step is no longer that one
    /// refuses and changes nothing.
    pub fn demote_context_with_shadows(
        &self,
        id: ContextId,
        judges: Option<&[String]>,
        archives: bool,
    ) -> KernelDbResult<(DemoteOutcome, Vec<ContextId>)> {
        let (promoted_at, demoted_at, archived_at): (Option<i64>, Option<i64>, Option<i64>) = self
            .conn
            .query_row(
                "SELECT promoted_at, demoted_at, archived_at FROM contexts WHERE context_id = ?1",
                params![blob_param(id.as_bytes())],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| KernelDbError::NotFound(format!("context {}", id.short())))?;

        if archived_at.is_some() {
            return Err(KernelDbError::Validation(
                "cannot demote an archived context".to_string(),
            ));
        }
        if demoted_at.is_some() != archives {
            let name = self.context_name(id);
            return Err(KernelDbError::Validation(if archives {
                format!(
                    "context {name} is no longer demoted, so its next step does not archive it; \
                     run `kj context demote <ctx>` again"
                )
            } else {
                format!(
                    "context {name} was demoted while this demote ran, so its next step archives it; \
                     run `kj context demote <ctx> --confirm` to archive it"
                )
            }));
        }

        if demoted_at.is_some() {
            let archived = self.archive_with_shadows(id, judges)?;
            Ok((DemoteOutcome::Archived, archived.shadows))
        } else if promoted_at.is_some() {
            self.conn.execute(
                "UPDATE contexts SET promoted_at = NULL WHERE context_id = ?1",
                params![blob_param(id.as_bytes())],
            )?;
            Ok((DemoteOutcome::Unpromoted, Vec::new()))
        } else {
            self.conn.execute(
                "UPDATE contexts SET demoted_at = ?1 WHERE context_id = ?2",
                params![now_millis(), blob_param(id.as_bytes())],
            )?;
            Ok((DemoteOutcome::Demoted, Vec::new()))
        }
    }

    /// Set or clear a context's `paused_at`. Design-only for now — see the
    /// doc on [`ContextRow::paused_at`] for the intended (not yet wired)
    /// gating semantics. Unlike `promoted_at`'s first-write-wins, this is a
    /// plain on/off flag: it unconditionally overwrites on every call.
    /// Refuses archived contexts loudly (symmetric with demote's guard);
    /// unknown ids stay `NotFound`.
    pub fn set_context_paused(&self, id: ContextId, paused: bool) -> KernelDbResult<()> {
        let value = if paused { Some(now_millis()) } else { None };
        let updated = self.conn.execute(
            "UPDATE contexts SET paused_at = ?1
             WHERE context_id = ?2 AND archived_at IS NULL",
            params![value, blob_param(id.as_bytes())],
        )?;
        if updated == 0 {
            // Distinguish "no such row" from "row exists but is archived" —
            // conflating them would misreport a real context as unknown.
            let archived: Option<bool> = self
                .conn
                .query_row(
                    "SELECT archived_at IS NOT NULL FROM contexts WHERE context_id = ?1",
                    params![blob_param(id.as_bytes())],
                    |row| row.get(0),
                )
                .optional()?;
            let verb = if paused { "pause" } else { "resume" };
            return Err(match archived {
                Some(true) => {
                    KernelDbError::Validation(format!("cannot {verb} an archived context"))
                }
                _ => KernelDbError::NotFound(format!("context {}", id.short())),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
