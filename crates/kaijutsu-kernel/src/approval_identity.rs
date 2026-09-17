//! Resolve approval reviewers from explicit context assignment, delegation, and the context forest.

use kaijutsu_types::{ContextId, PrincipalId};

use crate::kernel::Kernel;
use crate::llm::CharacterIdentity;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewSource {
    Explicit,
    Delegation,
    /// The responsible character of the nearest context at or above the
    /// ask's own, walking `forked_from` (`docs/character.md`, "Roots and
    /// rotation").
    Walk,
    /// Every layer is exhausted and the actor is a live root character:
    /// it is at its own root and confirms its own statement.
    SelfConfirmation,
}

impl ReviewSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Delegation => "delegation",
            Self::Walk => "walk",
            Self::SelfConfirmation => "self_confirmation",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedContextReview {
    pub reviewer: CharacterIdentity,
    pub source: ReviewSource,
}

impl Kernel {
    /// Resolve this context's effective approval reviewer, chained above its
    /// own configured performer (`played_by`) when one is set.
    pub async fn resolve_context_review(
        &self,
        context_id: ContextId,
    ) -> Result<ResolvedContextReview, String> {
        let row = self.kernel_db().lock().get_context(context_id)
            .map_err(|error| format!("could not read context review assignment: {error}"))?
            .ok_or_else(|| format!("no such context: {context_id}"))?;
        self.resolve_review_assignment(Some(context_id), row.played_by, row.director_id, row.reviewer_id).await
    }

    /// Resolve `context_id`'s reviewer for `actor` specifically, rather than
    /// the context's own configured performer — needed where the actor
    /// performing this particular invocation is not the context's
    /// `played_by`, e.g. a human's direct shell command in a context with no
    /// model performer set (`docs/approval-identity.md`, "Three identities").
    pub async fn resolve_context_review_for(
        &self,
        context_id: ContextId,
        actor: PrincipalId,
    ) -> Result<ResolvedContextReview, String> {
        let row = self.kernel_db().lock().get_context(context_id)
            .map_err(|error| format!("could not read context review assignment: {error}"))?
            .ok_or_else(|| format!("no such context: {context_id}"))?;
        self.resolve_review_assignment(Some(context_id), Some(actor), row.director_id, row.reviewer_id).await
    }

    /// Resolve a hypothetical context assignment before committing it.
    ///
    /// Order: explicit override, the director's delegation, the walk up
    /// `forked_from` from `context` (`docs/character.md`, "Roots and
    /// rotation"). The walk needs both a context to start from and a known
    /// actor to exclude; `context` is `None` only where the context does
    /// not exist yet, and a create passes its parent, which is where the
    /// walk would continue anyway. The walk never returns `actor`. Past it,
    /// a live root actor confirms its own statement and any other actor
    /// has no reviewer, which is an error.
    pub async fn resolve_review_assignment(
        &self,
        context: Option<ContextId>,
        actor: Option<PrincipalId>,
        director: Option<PrincipalId>,
        explicit_reviewer: Option<PrincipalId>,
    ) -> Result<ResolvedContextReview, String> {
        let (reviewer_id, source) = if let Some(reviewer) = explicit_reviewer {
            (reviewer, ReviewSource::Explicit)
        } else if let Some(director) = director {
            let delegation = self.kernel_db().lock().active_approval_delegation(director)
                .map_err(|error| format!("could not read approval delegation: {error}"))?;
            match delegation {
                Some(reviewer) => (reviewer, ReviewSource::Delegation),
                None => self.walk_then_root(context, actor)?,
            }
        } else {
            self.walk_then_root(context, actor)?
        };

        let reviewer = self.live_character(reviewer_id, "reviewer")?;
        Ok(ResolvedContextReview { reviewer, source })
    }

    /// The walk and self-confirmation layers of
    /// [`Self::resolve_review_assignment`].
    fn walk_then_root(
        &self,
        context: Option<ContextId>,
        actor: Option<PrincipalId>,
    ) -> Result<(PrincipalId, ReviewSource), String> {
        if let (Some(context), Some(actor)) = (context, actor) {
            let responsible = self.kernel_db().lock().responsible_character_above(context, &[actor])
                .map_err(|error| format!("could not walk the accountability forest: {error}"))?;
            if let Some(responsible) = responsible {
                return Ok((responsible, ReviewSource::Walk));
            }
        }
        let Some(actor) = actor else {
            return Err("no reviewer: the context has no explicit reviewer or delegation, and there is no actor to walk above".into());
        };
        let sheet = self.kernel_db().lock().get_character(actor)
            .map_err(|error| format!("could not read the actor's character sheet: {error}"))?;
        match sheet {
            Some(sheet) if sheet.root && sheet.retired_at.is_none() => Ok((actor, ReviewSource::SelfConfirmation)),
            sheet => {
                let name = sheet.map_or_else(|| actor.short(), |sheet| sheet.name);
                Err(format!(
                    "nobody reviews {name}: no one is responsible above it, and only a live root character confirms its own statements; assign one with `kj context set <context> --reviewer <character>`"
                ))
            }
        }
    }

    /// Require a model performer to differ from its resolved reviewer.
    pub async fn validate_model_review_assignment(
        &self,
        context: Option<ContextId>,
        performer: PrincipalId,
        director: Option<PrincipalId>,
        explicit_reviewer: Option<PrincipalId>,
    ) -> Result<ResolvedContextReview, String> {
        let review = self.resolve_review_assignment(context, Some(performer), director, explicit_reviewer).await?;
        if review.reviewer.principal_id == performer {
            return Err("the performer cannot review its own work; assign a different reviewer".into());
        }
        Ok(review)
    }

    fn live_character(&self, principal: PrincipalId, purpose: &str) -> Result<CharacterIdentity, String> {
        let character = self.kernel_db().lock().get_character(principal)
            .map_err(|error| format!("could not resolve {purpose}: {error}"))?
            .ok_or_else(|| format!("the assigned {purpose} {principal} has no character sheet"))?;
        if character.retired_at.is_some() {
            return Err(format!("the assigned {purpose} '{}' is retired", character.name));
        }
        Ok(CharacterIdentity { principal_id: character.principal_id, name: character.name })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_store::shared_block_store;
    use crate::kernel_db::KernelDb;
    use std::path::Path;
    use std::sync::Arc;

    async fn kernel_with_characters(names: &[&str]) -> (Kernel, Vec<PrincipalId>) {
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::temporary().unwrap()));
        let ids = names.iter().map(|name| {
            let principal_id = PrincipalId::new();
            db.lock().insert_character(&crate::kernel_db::CharacterRow { principal_id, name: (*name).to_string(), created_at: 0, retired_at: None, handoff_ctx: None, root_ctx: None, root: false }).unwrap();
            principal_id
        }).collect();
        let kernel = Kernel::new("approval-test", Path::new("/tmp"), shared_block_store(PrincipalId::system()), db).await;
        (kernel, ids)
    }

    #[tokio::test]
    async fn exhausted_walk_confirms_a_root_and_refuses_anyone_else() {
        let (kernel, ids) = kernel_with_characters(&["amy", "coder"]).await;
        let error = kernel.resolve_review_assignment(None, Some(ids[1]), None, None).await.unwrap_err();
        assert!(error.contains("coder") && error.contains("root"), "{error}");
        assert!(kernel.resolve_review_assignment(None, Some(ids[0]), None, None).await.is_err(), "amy is not a root yet");
        kernel.kernel_db().lock().update_character_root(ids[0], true).unwrap();
        let review = kernel.resolve_review_assignment(None, Some(ids[0]), None, None).await.unwrap();
        assert_eq!((review.reviewer.principal_id, review.source), (ids[0], ReviewSource::SelfConfirmation));
        assert!(kernel.resolve_review_assignment(None, None, None, None).await.is_err(), "no actor, no walk, no reviewer");
    }

    #[tokio::test]
    async fn explicit_override_precedes_director_grant_and_revoke_restores_the_walk() {
        let (kernel, ids) = kernel_with_characters(&["amy", "lead", "judge", "coder", "specialist"]).await;
        let [amy, lead, judge, coder, specialist] = ids.as_slice() else { unreachable!() };
        let error = kernel.resolve_review_assignment(None, Some(*coder), Some(*lead), None).await.unwrap_err();
        assert!(error.contains("coder"), "{error}");
        kernel.kernel_db().lock().grant_approval_delegation(*lead, *judge, *amy).unwrap();
        let review = kernel.resolve_review_assignment(None, Some(*coder), Some(*lead), None).await.unwrap();
        assert_eq!((review.reviewer.principal_id, review.source), (*judge, ReviewSource::Delegation));
        let review = kernel.resolve_review_assignment(None, Some(*coder), Some(*lead), Some(*specialist)).await.unwrap();
        assert_eq!((review.reviewer.principal_id, review.source), (*specialist, ReviewSource::Explicit));
        kernel.kernel_db().lock().revoke_approval_delegation(*lead, *amy).unwrap();
        assert!(kernel.resolve_review_assignment(None, Some(*coder), Some(*lead), None).await.is_err());
    }

    #[tokio::test]
    async fn director_may_review_distinct_coder_but_never_its_own_model_work() {
        let (kernel, ids) = kernel_with_characters(&["amy", "lead", "coder"]).await;
        kernel.kernel_db().lock().update_character_root(ids[0], true).unwrap();
        kernel.kernel_db().lock().grant_approval_delegation(ids[1], ids[1], ids[0]).unwrap();
        assert!(kernel.validate_model_review_assignment(None, ids[2], Some(ids[1]), None).await.is_ok());
        let error = kernel.validate_model_review_assignment(None, ids[1], Some(ids[1]), None).await.unwrap_err();
        assert!(error.contains("cannot review its own work"), "{error}");
        // A root confirms its own direct statements, but is never a model performer.
        assert!(kernel.resolve_review_assignment(None, Some(ids[0]), None, None).await.is_ok());
        assert!(kernel.validate_model_review_assignment(None, ids[0], None, None).await.is_err());
    }

    #[tokio::test]
    async fn missing_and_retired_reviewers_are_errors_without_fallback() {
        let (kernel, ids) = kernel_with_characters(&["amy", "lead", "judge", "coder"]).await;
        let error = kernel.resolve_review_assignment(None, Some(ids[3]), None, Some(PrincipalId::new())).await.unwrap_err();
        assert!(error.contains("no character sheet"), "{error}");
        kernel.kernel_db().lock().grant_approval_delegation(ids[1], ids[2], ids[0]).unwrap();
        kernel.kernel_db().lock().conn_for_ledger().execute(
            "UPDATE characters SET retired_at = 1 WHERE principal_id = ?1", [ids[2].as_bytes().as_slice()],
        ).unwrap();
        for director in [None, Some(ids[1])] {
            let explicit = if director.is_none() { Some(ids[2]) } else { None };
            let error = kernel.resolve_review_assignment(None, Some(ids[3]), director, explicit).await.unwrap_err();
            assert!(error.contains("retired"), "{error}");
        }
        kernel.kernel_db().lock().update_character_root(ids[0], true).unwrap();
        kernel.kernel_db().lock().conn_for_ledger().execute(
            "UPDATE characters SET retired_at = 1 WHERE principal_id = ?1", [ids[0].as_bytes().as_slice()],
        ).unwrap();
        let error = kernel.resolve_review_assignment(None, Some(ids[0]), None, None).await.unwrap_err();
        assert!(error.contains("amy"), "a retired root does not confirm itself: {error}");
    }

    #[tokio::test]
    async fn gate_resolves_after_revoke_instead_of_trusting_a_running_turn() {
        use crate::kj::{gate, gate_policy, test_helpers};
        let dispatcher = test_helpers::test_dispatcher().await;
        let amy = test_helpers::test_reviewer_principal();
        let lead = PrincipalId::new();
        let coder = PrincipalId::new();
        {
            let db = dispatcher.kernel_db().lock();
            for (principal_id, name) in [(amy, "amy"), (lead, "lead"), (coder, "coder")] {
                if db.get_character(principal_id).unwrap().is_none() {
                    db.insert_character(&crate::kernel_db::CharacterRow {
                        principal_id, name: name.into(), created_at: 0, retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
                    }).unwrap();
                }
            }
        }
        let context = test_helpers::register_rooted_context(&dispatcher, Some("coder-work"), amy);
        {
            let db = dispatcher.kernel_db().lock();
            db.update_context_review_assignment(context, Some(coder), None, Some(lead)).unwrap();
            db.grant_approval_delegation(lead, lead, amy).unwrap();
        }
        let before = dispatcher.kernel().resolve_context_review(context).await.unwrap();
        assert_eq!(before.reviewer.principal_id, lead);
        let stale_caller = test_helpers::caller_with_context(context).with_actor(coder, Some(lead));
        dispatcher.kernel_db().lock().revoke_approval_delegation(lead, amy).unwrap();
        let spec = || gate::GateSpec {
            publishes_pair: false, origin: approval_ledger::types::Origin::Hook,
            instance: "builtin.test".into(), tool: "approval-policy-test".into(), hook_id: None,
            description: "review work after delegation changed".into(), authorized_label: "work".into(),
            statements: vec![gate::GatedStatement {
                rendered: "work".into(), statement_kind: "test".into(), vars: vec![], source_index: None,
            }], exec_source: None, exec_stdin: None, planned: vec![],
        };
        let result = gate::run_gate(dispatcher.kernel(), &stale_caller, spec(), dispatcher.kernel().ledger_flows(), &gate_policy::no_config()).await;
        let request = result.ask.expect("new ask is routed after revocation");
        let row = dispatcher.kernel_db().lock().get_approval(&request.request_id).unwrap().unwrap();
        assert_eq!(row.reviewer_id.as_deref(), Some(amy.as_bytes().as_slice()));
        assert_eq!(row.actor_id.as_deref(), Some(coder.as_bytes().as_slice()));

        let unknown = test_helpers::caller_with_context(ContextId::new()).with_actor(coder, Some(lead));
        let result = gate::run_gate(dispatcher.kernel(), &unknown, spec(), dispatcher.kernel().ledger_flows(), &gate_policy::no_config()).await;
        assert!(result.ask.is_none(), "a missing context must not use the stale caller's reviewer");
        assert_eq!(dispatcher.kernel_db().lock().list_pending_asks().unwrap().len(), 1);
    }
}
