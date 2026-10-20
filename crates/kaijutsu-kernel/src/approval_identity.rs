//! Resolve approval reviewers from the senpai edge (`docs/senpai-kohai.md`).

use kaijutsu_types::{ContextId, PrincipalId};

use crate::kernel::Kernel;
use crate::kernel_db::{ReviewFrom, ReviewQuery};
use crate::llm::CharacterIdentity;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewSource {
    /// The holder of the nearest senpai context that is not the actor
    /// (`docs/senpai-kohai.md`, "Routing").
    Senpai,
    /// The climb reached a context with no senpai and the actor is a live
    /// root character: it confirms its own statement.
    SelfConfirmation,
}

impl ReviewSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Senpai => "senpai",
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
    /// Resolve this context's reviewer for its own configured performer
    /// (`played_by`). A context with no performer has no actor to route for;
    /// that is an error, not a guess.
    pub async fn resolve_context_review(
        &self,
        context_id: ContextId,
    ) -> Result<ResolvedContextReview, String> {
        let row = self.kernel_db().lock().get_context(context_id)
            .map_err(|error| format!("could not read context review assignment: {error}"))?
            .ok_or_else(|| format!("no such context: {context_id}"))?;
        let Some(performer) = row.played_by else {
            return Err(format!(
                "no reviewer: context {} has no performer, so there is no actor to route for",
                context_id.short()
            ));
        };
        self.resolve_review(ReviewFrom::Context(context_id), performer)
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
        self.resolve_review(ReviewFrom::Context(context_id), actor)
    }

    /// Require a model performer to differ from its resolved reviewer, for a
    /// context that exists or one about to be written under a senpai.
    pub async fn validate_model_review_assignment(
        &self,
        from: ReviewFrom,
        performer: PrincipalId,
    ) -> Result<ResolvedContextReview, String> {
        let review = self.resolve_review(from, performer)?;
        if review.reviewer.principal_id == performer {
            return Err("the performer cannot review its own work; assign a different reviewer".into());
        }
        Ok(review)
    }

    /// The one place the kernel asks [`crate::kernel_db::KernelDb::route_review`]
    /// for a reviewer and checks the answer names a live character.
    fn resolve_review(&self, from: ReviewFrom, actor: PrincipalId) -> Result<ResolvedContextReview, String> {
        let routed = self.kernel_db().lock().route_review(ReviewQuery { from, actor, past: None, confirm_at_top: true })
            .map_err(|error| error.to_string())?;
        let source = match routed {
            crate::kernel_db::EffectiveReviewer::Assigned(_) => ReviewSource::Senpai,
            crate::kernel_db::EffectiveReviewer::SelfConfirmation(_) => ReviewSource::SelfConfirmation,
        };
        let reviewer = self.live_character(routed.reviewer(), "reviewer")?;
        Ok(ResolvedContextReview { reviewer, source })
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
    use crate::kernel_db::{ContextRow, KernelDb};
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

    /// A live context under `senpai`, played by `played_by`.
    fn context(kernel: &Kernel, label: &str, senpai: Option<ContextId>, played_by: Option<PrincipalId>) -> ContextId {
        let context_id = ContextId::new();
        let db = kernel.kernel_db().lock();
        let ws = db.get_or_create_default_workspace(PrincipalId::system()).unwrap();
        db.insert_context_with_document(&ContextRow {
            context_id, label: Some(label.into()), provider: None, model: None, system_prompt: None,
            context_state: kaijutsu_types::ContextState::Live, context_type: "default".into(),
            created_at: kaijutsu_types::now_millis() as i64, created_by: PrincipalId::system(),
            forked_from: None, fork_kind: None, archived_at: None, workspace_id: None, preset_id: None,
            concluded_at: None, last_activity_at: None, promoted_at: None, demoted_at: None, paused_at: None,
            cast_id: None, origin_host: None, played_by, senpai_ctx: senpai,
        }, ws).unwrap();
        if let Some(performer) = played_by {
            db.update_played_by(context_id, Some(performer)).unwrap();
        }
        context_id
    }

    fn instance_of(kernel: &Kernel, context: ContextId) -> kaijutsu_types::CharacterInstanceId {
        kernel.kernel_db().lock().context_instance_row(context).unwrap().expect("the fixture gave the context an instance").instance_id
    }

    #[tokio::test]
    async fn a_root_console_confirms_its_root_and_refuses_anyone_else() {
        let (kernel, ids) = kernel_with_characters(&["amy", "coder"]).await;
        let top = context(&kernel, "top", None, Some(ids[0]));
        let error = kernel.resolve_context_review_for(top, ids[1]).await.unwrap_err();
        assert!(error.contains("coder") && error.contains("top") && error.contains("root"), "{error}");
        assert!(kernel.resolve_context_review_for(top, ids[0]).await.is_err(), "amy is not a root yet");
        kernel.kernel_db().lock().update_character_root(ids[0], true).unwrap();
        let review = kernel.resolve_context_review_for(top, ids[0]).await.unwrap();
        assert_eq!((review.reviewer.principal_id, review.source), (ids[0], ReviewSource::SelfConfirmation));
        let review = kernel.resolve_context_review(top).await.unwrap();
        assert_eq!((review.reviewer.principal_id, review.source), (ids[0], ReviewSource::SelfConfirmation), "its own performer routes the same way");

        let bare = context(&kernel, "bare", None, None);
        let error = kernel.resolve_context_review(bare).await.unwrap_err();
        assert!(error.contains("no performer"), "a context nobody plays has no actor to route for: {error}");
        let error = kernel.resolve_context_review_for(bare, ids[0]).await.unwrap_err();
        assert!(error.contains("bare") && error.contains("no character instance"), "a context with no instance routes nothing, even for a root: {error}");
    }

    #[tokio::test]
    async fn the_senpais_holder_reviews_and_a_retired_or_unnamed_holder_refuses() {
        let (kernel, ids) = kernel_with_characters(&["amy", "lead", "coder"]).await;
        let [amy, lead, coder] = ids.as_slice() else { unreachable!() };
        kernel.kernel_db().lock().update_character_root(*amy, true).unwrap();
        let root = context(&kernel, "amy", None, Some(*amy));
        let seat = context(&kernel, "lead-seat", Some(root), Some(*lead));
        let lane = context(&kernel, "coder-lane", Some(seat), Some(*coder));
        let review = kernel.resolve_context_review(lane).await.unwrap();
        assert_eq!((review.reviewer.principal_id, review.source), (*lead, ReviewSource::Senpai));
        let review = kernel.resolve_context_review_for(seat, *lead).await.unwrap();
        assert_eq!((review.reviewer.principal_id, review.source), (*amy, ReviewSource::Senpai));

        kernel.kernel_db().lock().conn_for_ledger().execute(
            "UPDATE characters SET retired_at = 1 WHERE principal_id = ?1", [lead.as_bytes().as_slice()],
        ).unwrap();
        let error = kernel.resolve_context_review(lane).await.unwrap_err();
        assert!(error.contains("lead") && error.contains("retired"), "{error}");

        // A context with no instance is not a senpai: the child below it is a
        // top of its own, held by a non-root, and nobody reviews it.
        let unheld = context(&kernel, "unheld", Some(root), None);
        let child = context(&kernel, "child-of-unheld", Some(unheld), Some(*coder));
        let error = kernel.resolve_context_review(child).await.unwrap_err();
        assert!(error.contains("child-of-unheld") && error.contains("no senpai"), "{error}");
    }

    #[tokio::test]
    async fn a_root_resolves_but_is_never_a_model_performer() {
        let (kernel, ids) = kernel_with_characters(&["amy", "lead", "coder"]).await;
        let [amy, lead, coder] = ids.as_slice() else { unreachable!() };
        kernel.kernel_db().lock().update_character_root(*amy, true).unwrap();
        let root = context(&kernel, "amy", None, Some(*amy));
        let seat = context(&kernel, "lead-seat", Some(root), Some(*lead));
        assert!(kernel.validate_model_review_assignment(ReviewFrom::Senpai(instance_of(&kernel, seat)), *coder).await.is_ok());
        // The performer is the holder of its own senpai: the climb goes past it.
        let review = kernel.validate_model_review_assignment(ReviewFrom::Senpai(instance_of(&kernel, seat)), *lead).await.unwrap();
        assert_eq!(review.reviewer.principal_id, *amy);
        // A root confirms its own direct statements, but is never a model performer.
        assert!(kernel.resolve_context_review_for(root, *amy).await.is_ok());
        let error = kernel.validate_model_review_assignment(ReviewFrom::Context(root), *amy).await.unwrap_err();
        assert!(error.contains("cannot review its own work"), "{error}");
    }

    #[tokio::test]
    async fn gate_routes_new_asks_to_the_current_senpai_not_a_running_turns_snapshot() {
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
        let root = test_helpers::register_root_context(&dispatcher);
        let seat = test_helpers::register_context(&dispatcher, Some("lead-seat"), Some(root), amy);
        let context = test_helpers::register_context(&dispatcher, Some("coder-work"), Some(seat), amy);
        {
            let db = dispatcher.kernel_db().lock();
            db.assign_test_review(seat, Some(lead), None).unwrap();
            db.assign_test_review(context, Some(coder), None).unwrap();
        }
        let before = dispatcher.kernel().resolve_context_review(context).await.unwrap();
        assert_eq!(before.reviewer.principal_id, lead);
        let stale_caller = test_helpers::caller_with_context(context).with_actor(coder, Some(lead));
        dispatcher.kernel_db().lock().set_context_senpai(context, Some(root)).unwrap();
        let spec = || gate::GateSpec {
            publishes_pair: false, tool_call: None, origin: approval_ledger::types::Origin::Hook,
            instance: "builtin.test".into(), tool: "approval-policy-test".into(), hook_id: None,
            description: "review work after the senpai changed".into(), authorized_label: "work".into(),
            statements: vec![gate::GatedStatement {
                rendered: "work".into(), statement_kind: "test".into(), vars: vec![], source_index: None,
            }], exec_source: None, exec_stdin: None, planned: vec![],
        };
        let result = gate::run_gate(dispatcher.kernel(), &stale_caller, spec(), dispatcher.kernel().ledger_flows(), &gate_policy::no_config()).await;
        let request = result.ask.expect("new ask is routed after the move");
        let row = dispatcher.kernel_db().lock().get_approval(&request.request_id).unwrap().unwrap();
        assert_eq!(row.reviewer_id.as_deref(), Some(amy.as_bytes().as_slice()));
        assert_eq!(row.actor_id.as_deref(), Some(coder.as_bytes().as_slice()));

        let unknown = test_helpers::caller_with_context(ContextId::new()).with_actor(coder, Some(lead));
        let result = gate::run_gate(dispatcher.kernel(), &unknown, spec(), dispatcher.kernel().ledger_flows(), &gate_policy::no_config()).await;
        assert!(result.ask.is_none(), "a missing context must not use the stale caller's reviewer");
        assert_eq!(dispatcher.kernel_db().lock().list_pending_asks().unwrap().len(), 1);
    }
}
