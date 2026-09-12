//! Resolve approval reviewers from explicit context assignment, delegation, and defaults.

use std::path::Path;

use serde::Deserialize;
use kaijutsu_types::{ContextId, PrincipalId};

use crate::kernel::Kernel;
use crate::llm::CharacterIdentity;
use crate::vfs::{VfsError, VfsOps};

const APPROVAL_CONFIG_FILE: &str = "approval.toml";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewSource {
    Explicit,
    Delegation,
    Default,
}

impl ReviewSource {
    pub fn as_str(self) -> &'static str {
        match self { Self::Explicit => "explicit", Self::Delegation => "delegation", Self::Default => "default" }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedContextReview {
    pub reviewer: CharacterIdentity,
    pub source: ReviewSource,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalConfig {
    default_reviewer: String,
}

impl ApprovalConfig {
    fn parse(text: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(text)
            .map_err(|error| format!("approval.toml parse error: {error}"))?;
        if config.default_reviewer.trim().is_empty() {
            return Err("approval.toml default_reviewer must not be empty".into());
        }
        Ok(config)
    }
}

impl Kernel {
    /// Resolve this context's effective approval reviewer.
    pub async fn resolve_context_review(
        &self,
        context_id: ContextId,
    ) -> Result<ResolvedContextReview, String> {
        let row = self.kernel_db().lock().get_context(context_id)
            .map_err(|error| format!("could not read context review assignment: {error}"))?
            .ok_or_else(|| format!("no such context: {context_id}"))?;
        self.resolve_review_assignment(row.played_by, row.director_id, row.reviewer_id).await
    }

    /// Resolve a hypothetical context assignment before committing it.
    pub async fn resolve_review_assignment(
        &self,
        _performer: Option<PrincipalId>,
        director: Option<PrincipalId>,
        explicit_reviewer: Option<PrincipalId>,
    ) -> Result<ResolvedContextReview, String> {
        // Refresh the default even when a higher-precedence route wins, so a
        // later gate cannot use a stale cached default after config breaks.
        // An explicit assignment or live director grant remains usable on its
        // own; only the default fallback needs this refresh to succeed.
        let default_reviewer = self.default_approval_reviewer().await;
        let (reviewer_id, source) = if let Some(reviewer) = explicit_reviewer {
            (reviewer, ReviewSource::Explicit)
        } else if let Some(director) = director {
            match self.kernel_db().lock().active_approval_delegation(director)
                .map_err(|error| format!("could not read approval delegation: {error}"))? {
                Some(reviewer) => (reviewer, ReviewSource::Delegation),
                None => (default_reviewer?, ReviewSource::Default),
            }
        } else {
            (default_reviewer?, ReviewSource::Default)
        };

        let reviewer = self.live_character(reviewer_id, "reviewer")?;
        Ok(ResolvedContextReview { reviewer, source })
    }

    /// Require a model performer to differ from its resolved reviewer.
    pub async fn validate_model_review_assignment(
        &self,
        performer: PrincipalId,
        director: Option<PrincipalId>,
        explicit_reviewer: Option<PrincipalId>,
    ) -> Result<ResolvedContextReview, String> {
        let review = self.resolve_review_assignment(Some(performer), director, explicit_reviewer).await?;
        if review.reviewer.principal_id == performer {
            return Err("the performer cannot review its own work; assign a different reviewer".into());
        }
        Ok(review)
    }

    pub async fn default_approval_reviewer(&self) -> Result<PrincipalId, String> {
        let result = self.load_default_approval_reviewer().await;
        let db = self.kernel_db().lock();
        match result {
            Ok(reviewer) => {
                db.set_default_approval_reviewer(reviewer)
                    .map_err(|error| format!("could not cache default reviewer: {error}"))?;
                Ok(reviewer)
            }
            Err(error) => {
                db.clear_default_approval_reviewer()
                    .map_err(|clear_error| format!("{error}; could not clear cached default reviewer: {clear_error}"))?;
                Err(error)
            }
        }
    }

    async fn load_default_approval_reviewer(&self) -> Result<PrincipalId, String> {
        let path = kaijutsu_types::paths::config_path(APPROVAL_CONFIG_FILE);
        let text = match self.vfs().read_all(Path::new(&path)).await {
            Ok(bytes) => String::from_utf8(bytes)
                .map_err(|error| format!("approval.toml is not valid UTF-8: {error}"))?,
            Err(VfsError::NotFound(_)) | Err(VfsError::NoMountPoint(_)) => {
                crate::config_seed::DEFAULT_APPROVAL_CONFIG.to_string()
            }
            Err(error) => return Err(format!("could not read {path}: {error}")),
        };
        let configured = ApprovalConfig::parse(&text)?.default_reviewer;
        let db = self.kernel_db().lock();
        let character = match PrincipalId::parse(&configured) {
            Ok(principal) => db.get_character(principal),
            Err(_) => db.get_character_by_name(&configured),
        }.map_err(|error| format!("could not resolve default reviewer '{configured}': {error}"))?
            .ok_or_else(|| format!("default reviewer '{configured}' has no character sheet"))?;
        if character.retired_at.is_some() {
            return Err(format!("default reviewer '{}' is retired", character.name));
        }
        Ok(character.principal_id)
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
    use std::sync::Arc;

    async fn kernel_with_characters(names: &[&str]) -> (Kernel, Vec<PrincipalId>) {
        let db = Arc::new(parking_lot::Mutex::new(KernelDb::temporary().unwrap()));
        let ids = names.iter().map(|name| {
            let principal_id = PrincipalId::new();
            db.lock().insert_character(&crate::kernel_db::CharacterRow { principal_id, name: (*name).to_string(), created_at: 0, retired_at: None, handoff_ctx: None }).unwrap();
            principal_id
        }).collect();
        let kernel = Kernel::new("approval-test", Path::new("/tmp"), shared_block_store(PrincipalId::system()), db).await;
        (kernel, ids)
    }

    #[tokio::test]
    async fn default_reviewer_is_amy_not_the_requester() {
        let (kernel, ids) = kernel_with_characters(&["amy", "coder"]).await;
        let review = kernel.resolve_review_assignment(Some(ids[1]), None, None).await.unwrap();
        assert_eq!(review.reviewer.principal_id, ids[0]);
        assert_eq!(review.source, ReviewSource::Default);
    }

    #[tokio::test]
    async fn explicit_override_precedes_director_grant_and_revoke_restores_default() {
        let (kernel, ids) = kernel_with_characters(&["amy", "lead", "judge", "coder", "specialist"]).await;
        let [amy, lead, judge, coder, specialist] = ids.as_slice() else { unreachable!() };
        let review = kernel.resolve_review_assignment(Some(*coder), Some(*lead), None).await.unwrap();
        assert_eq!((review.reviewer.principal_id, review.source), (*amy, ReviewSource::Default));
        kernel.kernel_db().lock().grant_approval_delegation(*lead, *judge, *amy).unwrap();
        let review = kernel.resolve_review_assignment(Some(*coder), Some(*lead), None).await.unwrap();
        assert_eq!((review.reviewer.principal_id, review.source), (*judge, ReviewSource::Delegation));
        let review = kernel.resolve_review_assignment(Some(*coder), Some(*lead), Some(*specialist)).await.unwrap();
        assert_eq!((review.reviewer.principal_id, review.source), (*specialist, ReviewSource::Explicit));
        kernel.kernel_db().lock().revoke_approval_delegation(*lead, *amy).unwrap();
        let review = kernel.resolve_review_assignment(Some(*coder), Some(*lead), None).await.unwrap();
        assert_eq!((review.reviewer.principal_id, review.source), (*amy, ReviewSource::Default));
    }

    #[tokio::test]
    async fn director_may_review_distinct_coder_but_never_its_own_model_work() {
        let (kernel, ids) = kernel_with_characters(&["amy", "lead", "coder"]).await;
        kernel.kernel_db().lock().grant_approval_delegation(ids[1], ids[1], ids[0]).unwrap();
        assert!(kernel.validate_model_review_assignment(ids[2], Some(ids[1]), None).await.is_ok());
        let error = kernel.validate_model_review_assignment(ids[1], Some(ids[1]), None).await.unwrap_err();
        assert!(error.contains("cannot review its own work"), "{error}");
        // Direct RPC resolves a reviewer without turning the connected human into a model.
        assert!(kernel.resolve_review_assignment(Some(ids[0]), None, None).await.is_ok());
        assert!(kernel.validate_model_review_assignment(ids[0], None, None).await.is_err());
    }

    #[tokio::test]
    async fn missing_and_retired_reviewers_are_errors_without_fallback() {
        let (kernel, ids) = kernel_with_characters(&["amy", "lead", "judge", "coder"]).await;
        let error = kernel.resolve_review_assignment(Some(ids[3]), None, Some(PrincipalId::new())).await.unwrap_err();
        assert!(error.contains("no character sheet"), "{error}");
        kernel.kernel_db().lock().grant_approval_delegation(ids[1], ids[2], ids[0]).unwrap();
        kernel.kernel_db().lock().conn_for_ledger().execute(
            "UPDATE characters SET retired_at = 1 WHERE principal_id = ?1", [ids[2].as_bytes().as_slice()],
        ).unwrap();
        for director in [None, Some(ids[1])] {
            let explicit = if director.is_none() { Some(ids[2]) } else { None };
            let error = kernel.resolve_review_assignment(Some(ids[3]), director, explicit).await.unwrap_err();
            assert!(error.contains("retired"), "{error}");
        }
        kernel.kernel_db().lock().conn_for_ledger().execute(
            "UPDATE characters SET retired_at = 1 WHERE principal_id = ?1", [ids[0].as_bytes().as_slice()],
        ).unwrap();
        assert!(kernel.default_approval_reviewer().await.unwrap_err().contains("retired"));
        let (missing, _) = kernel_with_characters(&["coder"]).await;
        assert!(missing.default_approval_reviewer().await.unwrap_err().contains("no character sheet"));
    }

    #[tokio::test]
    async fn config_selects_name_or_id_and_invalid_config_fails_loudly() {
        let (kernel, ids) = kernel_with_characters(&["amy", "judge"]).await;
        let config = tempfile::tempdir().unwrap();
        kernel.mount(kaijutsu_types::paths::CONFIG_ROOT, crate::vfs::LocalBackend::new(config.path())).await;
        let path = config.path().join("approval.toml");
        for selector in ["judge".to_string(), ids[1].to_string()] {
            std::fs::write(&path, format!("default_reviewer = \"{selector}\"\n")).unwrap();
            assert_eq!(kernel.default_approval_reviewer().await.unwrap(), ids[1]);
        }
        for invalid in ["default_reviewer = \"\"", "default_reviewer = \"amy\"\nunknown = true", "broken = ["] {
            std::fs::write(&path, invalid).unwrap();
            assert!(kernel.default_approval_reviewer().await.is_err(), "accepted {invalid}");
        }
        std::fs::write(&path, b"\xff").unwrap();
        assert!(kernel.default_approval_reviewer().await.unwrap_err().contains("UTF-8"));
    }

    #[tokio::test]
    async fn explicit_and_delegated_reviewers_survive_bad_default_without_a_stale_cache() {
        let (kernel, ids) = kernel_with_characters(&["amy", "lead", "judge", "coder", "specialist"]).await;
        let [amy, lead, judge, coder, specialist] = ids.as_slice() else { unreachable!() };
        let config = tempfile::tempdir().unwrap();
        kernel.mount(kaijutsu_types::paths::CONFIG_ROOT, crate::vfs::LocalBackend::new(config.path())).await;
        let path = config.path().join("approval.toml");
        std::fs::write(&path, "default_reviewer = \"amy\"\n").unwrap();
        assert_eq!(kernel.default_approval_reviewer().await.unwrap(), *amy);
        kernel.kernel_db().lock().grant_approval_delegation(*lead, *judge, *amy).unwrap();

        for invalid in ["default_reviewer = \"missing\"\n", "broken = ["] {
            std::fs::write(&path, invalid).unwrap();
            let explicit = kernel.resolve_review_assignment(Some(*coder), Some(*lead), Some(*specialist)).await.unwrap();
            assert_eq!((explicit.reviewer.principal_id, explicit.source), (*specialist, ReviewSource::Explicit));
            let delegated = kernel.resolve_review_assignment(Some(*coder), Some(*lead), None).await.unwrap();
            assert_eq!((delegated.reviewer.principal_id, delegated.source), (*judge, ReviewSource::Delegation));
            assert!(kernel.resolve_review_assignment(Some(*coder), None, None).await.is_err());
            assert_eq!(kernel.kernel_db().lock().cached_default_approval_reviewer().unwrap(), None);
        }

        std::fs::write(&path, "default_reviewer = \"amy\"\n").unwrap();
        kernel.kernel_db().lock().conn_for_ledger().execute(
            "UPDATE characters SET retired_at = 1 WHERE principal_id = ?1", [amy.as_bytes().as_slice()],
        ).unwrap();
        let explicit = kernel.resolve_review_assignment(Some(*coder), Some(*lead), Some(*specialist)).await.unwrap();
        assert_eq!((explicit.reviewer.principal_id, explicit.source), (*specialist, ReviewSource::Explicit));
        let delegated = kernel.resolve_review_assignment(Some(*coder), Some(*lead), None).await.unwrap();
        assert_eq!((delegated.reviewer.principal_id, delegated.source), (*judge, ReviewSource::Delegation));
        assert!(kernel.resolve_review_assignment(Some(*coder), None, None).await.is_err());
        assert_eq!(kernel.kernel_db().lock().cached_default_approval_reviewer().unwrap(), None);
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
                        principal_id, name: name.into(), created_at: 0, retired_at: None, handoff_ctx: None,
                    }).unwrap();
                }
            }
        }
        let context = test_helpers::register_context(&dispatcher, Some("coder-work"), None, amy);
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
            origin: approval_ledger::types::Origin::Hook,
            instance: "builtin.test".into(), tool: "approval-policy-test".into(), hook_id: None,
            description: "review work after delegation changed".into(), authorized_label: "work".into(),
            statements: vec![gate::GatedStatement {
                rendered: "work".into(), statement_kind: "test".into(), vars: vec![], source_index: None,
            }], exec_source: None, planned: vec![],
        };
        let result = gate::run_gate(dispatcher.kernel_db(), &stale_caller, spec(), dispatcher.kernel().ledger_flows(), &gate_policy::no_config()).await;
        let request = result.ask.expect("new ask is routed after revocation");
        let row = dispatcher.kernel_db().lock().get_approval(&request.request_id).unwrap().unwrap();
        assert_eq!(row.reviewer_id.as_deref(), Some(amy.as_bytes().as_slice()));
        assert_eq!(row.actor_id.as_deref(), Some(coder.as_bytes().as_slice()));

        let unknown = test_helpers::caller_with_context(ContextId::new()).with_actor(coder, Some(lead));
        let result = gate::run_gate(dispatcher.kernel_db(), &unknown, spec(), dispatcher.kernel().ledger_flows(), &gate_policy::no_config()).await;
        assert!(result.ask.is_none(), "a missing context must not use the stale caller's reviewer");
        assert_eq!(dispatcher.kernel_db().lock().list_pending_asks().unwrap().len(), 1);
    }
}
