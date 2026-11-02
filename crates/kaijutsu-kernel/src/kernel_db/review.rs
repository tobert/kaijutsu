//! Senpai chain, review routing, and authority.

use super::*;

/// Where a review route starts (`docs/senpai-kohai.md`, "Routing").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewFrom {
    /// An ask raised in this existing context: routing starts at its senpai.
    Context(ContextId),
    /// A context not yet written whose senpai will be this context (a create
    /// before the insert): routing starts at that senpai.
    Senpai(ContextId),
}

/// An act the one authority rule governs (`docs/senpai-kohai.md`, "Authority").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GovernedAct {
    /// `kj context set <ctx> --egress-allow|--egress-deny`.
    Egress,
    /// `kj context move`.
    Move,
    /// `kj ledger escalate` on an ask raised in the context.
    Escalate,
    /// `kj context rotate` (its own performer may also rotate it).
    Rotate,
    /// `kj context archive`.
    Archive,
}

impl GovernedAct {
    pub const ALL: [GovernedAct; 5] = [Self::Egress, Self::Move, Self::Escalate, Self::Rotate, Self::Archive];

    /// The act, as the refusal names it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Egress => "change its egress list",
            Self::Move => "move it to another senpai",
            Self::Escalate => "escalate its asks",
            Self::Rotate => "rotate it",
            Self::Archive => "archive it",
        }
    }
}

/// Which half of the authority rule matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authority {
    /// The actor holds the context's senpai.
    SenpaiHolder,
    /// The actor is the context's lineage root.
    LineageRoot,
    /// The actor performs the context and archives it.
    OwnPerformer,
}

/// One request to [`KernelDb::route_review`].
#[derive(Debug, Clone, Copy)]
pub struct ReviewQuery {
    pub from: ReviewFrom,
    /// The character performing; it never reviews its own work.
    pub actor: PrincipalId,
    /// Climb past the context held by this reviewer before landing: an
    /// escalation, or the reviewer-context chain, asks for the holder above
    /// the one it already has. The reviewer must hold a senpai on the chain.
    pub past: Option<PrincipalId>,
    /// Whether a live root `actor` confirms its own statement when the climb
    /// reaches a context with no senpai. Routing an ask does; an escalation
    /// or a chain walk instead refuses there, naming the context.
    pub confirm_at_top: bool,
}

impl ReviewQuery {
    /// The reviewer of an ask `actor` raises in `context`.
    pub fn ask(context: ContextId, actor: PrincipalId) -> Self {
        Self { from: ReviewFrom::Context(context), actor, past: None, confirm_at_top: true }
    }

    /// The reviewer of an ask `actor` would raise in a context whose senpai is `senpai`.
    pub fn under(senpai: ContextId, actor: PrincipalId) -> Self {
        Self { from: ReviewFrom::Senpai(senpai), actor, past: None, confirm_at_top: true }
    }

    /// The holder above `reviewer` on the senpai chain: climb past the
    /// context `reviewer` holds, and refuse rather than self-confirm at the top.
    pub fn above(mut self, reviewer: PrincipalId) -> Self {
        self.past = Some(reviewer);
        self.confirm_at_top = false;
        self
    }
}

/// Outcome of [`KernelDb::route_review`]. Distinct from a
/// `KernelDbResult` error: an exhausted walk for a root actor is an
/// expected terminal state, not a ledger fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveReviewer {
    /// A live character, distinct from the acting one, reviews its work.
    Assigned(PrincipalId),
    /// Every resolution layer is exhausted for a live root actor: no
    /// explicit override or delegation applies, and no context at or above
    /// this one has a live responsible character other than the actor. The
    /// actor is at its own root, so it reviews its own work: the ask is
    /// raised with the actor as reviewer and only the actor may answer it
    /// (`docs/approval-identity.md`).
    SelfConfirmation(PrincipalId),
    /// A non-root actor acts in a root context the live root holds, and that
    /// root reviews it. The context has no senpai: the holder is the root
    /// itself.
    TopHolder(PrincipalId),
}

impl EffectiveReviewer {
    /// The reviewer to snapshot on an ask, either way. A self-confirmation
    /// names the actor.
    pub fn reviewer(self) -> PrincipalId {
        match self {
            Self::Assigned(reviewer) | Self::TopHolder(reviewer) => reviewer,
            Self::SelfConfirmation(actor) => actor,
        }
    }
}

impl KernelDb {
    /// The character that answers for `context`: its performer
    /// (`docs/senpai-kohai.md`, "The model"). `None` when the context has none.
    pub fn context_holder(&self, context: ContextId) -> KernelDbResult<Option<PrincipalId>> {
        Ok(self.context_character(context)?.map(|cc| cc.principal_id))
    }

    /// `label (short-id)`, for messages that name a context.
    pub(super) fn context_display(row: &ContextRow) -> String {
        match &row.label {
            Some(label) => format!("{label} ({})", row.context_id.short()),
            None => row.context_id.short(),
        }
    }

    /// The senpai context of `context`, or `None` for a root console (or a
    /// context with no row).
    pub fn senpai_of(&self, context: ContextId) -> KernelDbResult<Option<ContextId>> {
        Ok(self
            .conn
            .query_row(
                "SELECT senpai_ctx FROM contexts WHERE context_id = ?1",
                params![blob_param(context.as_bytes())],
                |r| read_opt_context_id(r, 0),
            )
            .optional()?
            .flatten())
    }

    /// The nearest performer on the senpai chain strictly above `context`,
    /// climbing past senpais with no performer: the context that holds it and
    /// the performer. `None` when the chain ends without one. A senpai cycle is
    /// corruption and refuses.
    pub fn senpai_holder(&self, context: ContextId) -> KernelDbResult<Option<(ContextId, PrincipalId)>> {
        let mut visited = HashSet::from([context]);
        let mut at = self.senpai_of(context)?;
        while let Some(senpai) = at {
            if !visited.insert(senpai) {
                return Err(KernelDbError::Corrupt(format!(
                    "context {} is its own senpai ancestor; the senpai chain has a cycle",
                    self.context_name(senpai)
                )));
            }
            let row = self
                .get_context(senpai)?
                .ok_or_else(|| KernelDbError::NotFound(format!("senpai context {}", senpai.short())))?;
            if let Some(performer) = row.performer {
                return Ok(Some((senpai, performer)));
            }
            at = row.senpai_ctx;
        }
        Ok(None)
    }

    /// The live kohai of `context`: the contexts whose senpai it is, oldest
    /// first. A context with no kohai (or no context character) lists none.
    pub fn list_kohai(&self, context: ContextId) -> KernelDbResult<Vec<ContextRow>> {
        let mut kohai = Vec::new();
        for id in self.kohai_of(context)? {
            kohai.push(self.get_context(id)?.ok_or_else(|| {
                KernelDbError::NotFound(format!("kohai {} of context {}", id.short(), context.short()))
            })?);
        }
        Ok(kohai)
    }

    /// Route a review: the one resolver behind every reviewer decision
    /// (`docs/senpai-kohai.md`, "Routing").
    ///
    /// Let K be the ask's context. The reviewer is the performer of K's
    /// senpai context when it is live and is neither the actor nor the
    /// reviewer `past`; a character that is, is climbed past to its own
    /// senpai, and so is a senpai with no performer. A retired character
    /// refuses by name: the climb never substitutes another. At a context with
    /// no senpai (a root console) the live root character that performs it
    /// confirms its own statement when `confirm_at_top`; otherwise the climb
    /// refuses, naming the actor and the context.
    ///
    /// A missing context, a senpai with no row and a senpai cycle are
    /// corruption and are errors, never an exhausted climb.
    pub fn route_review(&self, query: ReviewQuery) -> KernelDbResult<EffectiveReviewer> {
        let ReviewQuery { from, actor, past, confirm_at_top } = query;
        let mut passed = past.is_none();
        let (origin, mut below, mut senpai): (Option<ContextId>, Option<ContextRow>, Option<ContextId>) = match from {
            ReviewFrom::Context(context) => {
                let row = self.get_context(context)?.ok_or_else(|| {
                    KernelDbError::NotFound(format!("context {} has no row to resolve a reviewer from", context.short()))
                })?;
                let senpai = row.senpai_ctx;
                (Some(context), Some(row), senpai)
            }
            ReviewFrom::Senpai(context) => (None, None, Some(context)),
        };
        // The performer of the context the ask is raised in never reviews it,
        // whoever the actor is: a fork's performer is its source's holder.
        let performer = below.as_ref().and_then(|row| row.performer);
        let mut visited = HashSet::new();
        if let Some(row) = &below {
            visited.insert(row.context_id);
        }
        loop {
            let Some(senpai_id) = senpai else {
                let at = below.as_ref().expect("a climb with no senpai started at a context");
                if !passed {
                    let who = past.map_or_else(String::new, |p| self.name_for(p));
                    return Err(KernelDbError::NoReviewerAbove(format!(
                        "{who} holds no senpai context on the chain above {}; it was assigned by hand, so name the next reviewer explicitly",
                        self.context_name(at.context_id)
                    )));
                }
                return self.at_top(at, origin, actor, confirm_at_top);
            };
            if !visited.insert(senpai_id) {
                return Err(KernelDbError::Corrupt(format!(
                    "context {} is its own senpai ancestor; the senpai chain has a cycle",
                    self.context_name(senpai_id)
                )));
            }
            let above = self.get_context(senpai_id)?.ok_or_else(|| {
                KernelDbError::NotFound(format!("senpai context {} has no row, so the climb cannot continue", senpai_id.short()))
            })?;
            let under = below.as_ref().map_or_else(|| "the new context".to_string(), |b| self.context_name(b.context_id));
            let Some(holder) = above.performer else {
                senpai = above.senpai_ctx;
                below = Some(above);
                continue;
            };
            if !passed {
                // Still climbing to the context the current reviewer holds.
                passed = Some(holder) == past;
                senpai = above.senpai_ctx;
                below = Some(above);
                continue;
            }
            if holder == actor || Some(holder) == past || Some(holder) == performer {
                senpai = above.senpai_ctx;
                below = Some(above);
                continue;
            }
            return match self.get_character(holder)? {
                Some(sheet) if sheet.retired_at.is_none() => Ok(EffectiveReviewer::Assigned(holder)),
                Some(sheet) => Err(KernelDbError::Validation(format!(
                    "{} is {}, the senpai of {under}, and is retired; routing does not pass over a retired character: \
                     move {under} to another senpai with `kj context move`",
                    sheet.name, self.context_name(above.context_id)
                ))),
                None => Err(KernelDbError::Corrupt(format!(
                    "the character of {}, the senpai of {under}, has no character sheet",
                    self.context_name(above.context_id)
                ))),
            };
        }
    }

    /// The climb reached `at`, a context with no senpai. `origin` is the
    /// context the ask was raised in, when there is one.
    pub(super) fn at_top(&self, at: &ContextRow, origin: Option<ContextId>, actor: PrincipalId, confirm_at_top: bool) -> KernelDbResult<EffectiveReviewer> {
        let name = origin.map_or_else(|| self.context_name(at.context_id), |context| self.context_name(context));
        if !confirm_at_top {
            return Err(KernelDbError::NoReviewerAbove(format!(
                "context {} has no senpai, so nobody is above it",
                self.context_name(at.context_id)
            )));
        }
        match self.get_character(actor)? {
            Some(sheet) if sheet.root && sheet.retired_at.is_none() && at.performer == Some(actor) => {
                Ok(EffectiveReviewer::SelfConfirmation(actor))
            }
            Some(sheet) if !sheet.root => {
                let holder = match at.performer {
                    Some(performer) => self.get_character(performer)?.filter(|holder| holder.root && holder.retired_at.is_none()),
                    None => None,
                };
                match holder {
                    Some(holder) => Ok(EffectiveReviewer::TopHolder(holder.principal_id)),
                    None => Err(self.nobody_reviews(Some(sheet.name), actor, &name, at)),
                }
            }
            sheet => Err(self.nobody_reviews(sheet.map(|sheet| sheet.name), actor, &name, at)),
        }
    }

    fn nobody_reviews(&self, who: Option<String>, actor: PrincipalId, name: &str, at: &ContextRow) -> KernelDbError {
        let who = who.unwrap_or_else(|| actor.short());
        KernelDbError::Validation(format!(
            "nobody reviews {who} in context {name}: {} has no senpai above it, and only the live \
             root character holding a root console confirms its own statements; give the context a senpai \
             with `kj context move`",
            self.context_name(at.context_id)
        ))
    }

    /// The root character at the top of `context_id`'s senpai chain: the
    /// performer of the context the chain ends at. The inner `Err` says why
    /// the lineage has no live root; the outer `Err` is a fault.
    pub(super) fn lineage_root_of_context(&self, context_id: ContextId) -> KernelDbResult<Result<PrincipalId, String>> {
        let (top, performer) = self.lineage_top(context_id)?;
        let top_name = self.context_name(top);
        let Some(performer) = performer else {
            return Ok(Err(format!("the senpai chain ends at {top_name}, which is not a root console")));
        };
        Ok(match self.get_character(performer)? {
            Some(sheet) if !sheet.root => Err(format!(
                "{} holds {top_name} at the top of this senpai chain and is not a root character",
                sheet.name
            )),
            Some(sheet) if sheet.retired_at.is_some() => Err(format!(
                "root character {} is retired, so {top_name} has no lineage root",
                sheet.name
            )),
            Some(_) => Ok(performer),
            None => Err(format!("the character of {top_name} has no character sheet")),
        })
    }

    /// The lineage root of `context_id`, or the reason it has none.
    pub(super) fn lineage_root_or_reason(&self, context_id: ContextId) -> KernelDbResult<Result<PrincipalId, String>> {
        if self.get_context(context_id)?.is_none() {
            return Err(KernelDbError::NotFound(format!("context {}", context_id.short())));
        }
        self.lineage_root_of_context(context_id)
    }

    /// The root character at the top of `context_id`'s senpai chain. It
    /// holds routing authority over every context in that lineage
    /// (`docs/approval-identity.md`). A top held by a non-root or a retired
    /// root, a top with no performer, and a broken chain all refuse.
    pub fn lineage_root(&self, context_id: ContextId) -> KernelDbResult<PrincipalId> {
        match self.lineage_root_or_reason(context_id)? {
            Ok(root) => Ok(root),
            Err(reason) => Err(KernelDbError::Validation(reason)),
        }
    }

    /// The lineage root, or `None` when the lineage legitimately has no live
    /// root: the top is not a root console, is not a root, is retired, or has
    /// no sheet, and when `context_id` itself has no row
    /// (`approvals.context_id` has no foreign key, so an ask may outlive its
    /// context). A fault (a missing ancestor, a senpai cycle, a database
    /// error) is an `Err`, never `None`.
    pub fn live_lineage_root(&self, context_id: ContextId) -> KernelDbResult<Option<PrincipalId>> {
        if self.get_context(context_id)?.is_none() {
            return Ok(None);
        }
        Ok(self.lineage_root_or_reason(context_id)?.ok())
    }

    /// The root console a new performer-less context answers to when no
    /// senpai was named (`docs/senpai-kohai.md`, "Authority"): the console at the top of `origin`'s senpai chain, when
    /// `origin` is the context the creating act ran in and that console is a
    /// live root's; else the console of the only live root. A kernel with no
    /// live root console, or several and no usable `origin`, refuses.
    pub fn default_console_for(&self, origin: Option<ContextId>) -> KernelDbResult<ContextId> {
        if let Some(origin) = origin
            && self.get_context(origin)?.is_some()
        {
            let (top, performer) = self.lineage_top(origin)?;
            if let Some(performer) = performer
                && self.is_root_console(top)?
                && self.get_character(performer)?.is_some_and(|sheet| sheet.root && sheet.retired_at.is_none())
            {
                return Ok(top);
            }
        }
        let consoles: Vec<ContextId> = self
            .list_characters(false)?
            .into_iter()
            .filter(|sheet| sheet.root)
            .filter_map(|sheet| sheet.root_ctx)
            .collect();
        match consoles.as_slice() {
            [only] => Ok(*only),
            [] => Err(KernelDbError::Validation(
                "the kernel has no live root character with a console, so a new context has no senpai to answer to".into(),
            )),
            _ => Err(KernelDbError::Validation(format!(
                "the kernel has {} live roots and the context it was made from reaches none of their consoles, \
                 so a new context has no senpai to answer to; run it from a context under a root console",
                consoles.len()
            ))),
        }
    }

    /// The one authority rule (`docs/senpai-kohai.md`, "Authority"): the
    /// performer of `target`'s senpai (climbing past senpais with no
    /// performer), or the lineage root, may perform `act` on it. The context's
    /// own performer may also archive it. The inner `Err` is the refusal,
    /// naming the rule, the senpai's character and the root; the outer `Err` is
    /// a fault (a missing row, a senpai cycle). The context's own performer is
    /// refused for every other act; `kj context rotate` admits it before this
    /// check.
    pub fn context_authority(
        &self,
        target: ContextId,
        actor: PrincipalId,
        act: GovernedAct,
    ) -> KernelDbResult<Result<Authority, String>> {
        let row = self.get_context(target)?.ok_or_else(|| KernelDbError::NotFound(format!(
            "context {} has no row to check authority over", target.short()
        )))?;
        let holder = self.senpai_holder(target)?.map(|(_, performer)| performer);
        if holder == Some(actor) {
            return Ok(Ok(Authority::SenpaiHolder));
        }
        if act == GovernedAct::Archive && row.performer == Some(actor) {
            return Ok(Ok(Authority::OwnPerformer));
        }
        let root = self.lineage_root_of_context(target)?;
        if root.as_ref() == Ok(&actor) {
            return Ok(Ok(Authority::LineageRoot));
        }
        let holder_name = holder.map_or_else(|| "none".to_string(), |h| self.name_for(h));
        let root_name = match root {
            Ok(root) => self.name_for(root),
            Err(reason) => format!("none: {reason}"),
        };
        let own = match (act, row.performer) {
            (GovernedAct::Archive, Some(performer)) => format!(", its own performer ({}),", self.name_for(performer)),
            _ => String::new(),
        };
        Ok(Err(format!(
            "only {}'s senpai ({holder_name}){own} or its lineage root ({root_name}) may {}",
            self.context_name(target), act.as_str()
        )))
    }

    /// Whether `actor` may place a context under `senpai`: it performs
    /// `senpai` (or the nearest performer above it), or is `senpai`'s lineage
    /// root (`docs/senpai-kohai.md`, "Creating, forking, rotating"). The inner
    /// `Err` is the refusal.
    pub fn may_place_under(&self, senpai: ContextId, actor: PrincipalId) -> KernelDbResult<Result<Authority, String>> {
        let row = self.get_context(senpai)?.ok_or_else(|| KernelDbError::NotFound(format!(
            "senpai context {}", senpai.short()
        )))?;
        let holder = match row.performer {
            Some(performer) => Some(performer),
            None => self.senpai_holder(senpai)?.map(|(_, performer)| performer),
        };
        if holder == Some(actor) {
            return Ok(Ok(Authority::SenpaiHolder));
        }
        let root = self.lineage_root_of_context(senpai)?;
        if root.as_ref() == Ok(&actor) {
            return Ok(Ok(Authority::LineageRoot));
        }
        let holder_name = holder.map_or_else(|| "none".to_string(), |h| self.name_for(h));
        let root_name = match root {
            Ok(root) => self.name_for(root),
            Err(reason) => format!("none: {reason}"),
        };
        Ok(Err(format!(
            "only the holder of {} ({holder_name}) or its lineage root ({root_name}) may place a context under it",
            self.context_name(senpai)
        )))
    }
}

#[cfg(test)]
mod tests;
