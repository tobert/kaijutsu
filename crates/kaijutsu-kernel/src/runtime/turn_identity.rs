//! Resolve the performer and reviewer before starting model work.

use crate::Kernel;
use crate::approval_identity::ResolvedContextReview;
use crate::kernel_db::KernelDb;
use kaijutsu_types::{ContextId, PrincipalId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TurnIdentity {
    pub actor: PrincipalId,
    pub reviewer: PrincipalId,
}

/// A context a person performs runs no model turns (`docs/character.md`, "A
/// person, the console, MCP sessions, the judge"), so it is refused before
/// the person, often their own reviewer, meets the self-review refusal.
pub(crate) fn person_refusal(person: &str) -> String {
    format!("{person} is a person, and a context a person performs runs no model turns; its draft runs as shell. Talk to a model in the console.")
}

/// Refuse, with [`person_refusal`], a model turn in a context a person
/// performs. A verb that writes before it requests a turn (`kj drive
/// --prompt`, `kj fork --prompt`) checks this first, so the refusal leaves
/// nothing behind. A context with no row is the caller's to report.
pub(crate) fn refuse_a_persons_context(db: &KernelDb, context_id: ContextId) -> Result<(), String> {
    let row = db.get_context(context_id)
        .map_err(|e| format!("Could not read performer assignment: {e}"))?;
    let Some(performer) = row.and_then(|row| row.performer) else { return Ok(()) };
    match db.get_character(performer).map_err(|e| format!("Could not resolve performer: {e}"))? {
        Some(sheet) if sheet.root => Err(person_refusal(&sheet.name)),
        _ => Ok(()),
    }
}

/// The performer is checked before the reviewer resolves: the reviewer walk
/// starts above the performer, so with none set it fails with a message
/// about the reviewer, which is not the repair the caller needs.
pub(crate) fn require_performer(actor: Option<PrincipalId>) -> Result<PrincipalId, String> {
    actor.ok_or_else(||
        "No performer assigned: this context has no performer. A context gets its character when it is made, with 'kj context create <character>'.".to_string()
    )
}

pub(crate) fn resolve(
    db: &KernelDb,
    actor: Option<PrincipalId>,
    reviewer: PrincipalId,
) -> Result<TurnIdentity, String> {
    let actor = require_performer(actor)?;
    if actor == reviewer {
        return Err("The performer cannot review its own work. Ask the holder of this context's senpai, or its lineage root, to move it under a senpai another character holds with 'kj context set . --senpai <senpai-context>'.".to_string());
    }
    let character = db.get_character(actor)
        .map_err(|e| format!("Could not resolve performer: {e}"))?
        .ok_or_else(|| format!("The assigned performer {actor} has no character sheet."))?;
    if character.retired_at.is_some() {
        return Err(format!("The assigned performer '{}' is retired. Assign a live character before starting a model turn.", character.name));
    }
    Ok(TurnIdentity { actor, reviewer })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel_db::CharacterRow;

    fn character(db: &KernelDb, name: &str) -> PrincipalId {
        let principal_id = PrincipalId::new();
        db.insert_character(&CharacterRow { rc_dir: Some("coder".into()), console_ctx: None, mcp_hub_ctx: None, context_type: None,
            principal_id, name: name.into(), created_at: 0,
            retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        principal_id
    }

    #[test]
    fn coder_and_director_remain_distinct_characters() {
        let db = KernelDb::temporary().unwrap();
        let coder = character(&db, "coder");
        let lead = character(&db, "lead");
        assert_eq!(resolve(&db, Some(coder), lead).unwrap(), TurnIdentity { actor: coder, reviewer: lead });
    }

    #[test]
    fn missing_or_self_review_assignment_stops_the_turn() {
        let db = KernelDb::temporary().unwrap();
        let amy = character(&db, "amy");
        assert!(resolve(&db, None, amy).unwrap_err().contains("No performer"));
        assert!(resolve(&db, Some(amy), amy).unwrap_err().contains("cannot review"));
    }

    /// Every context a person performs runs no model turns, whatever its
    /// type and whoever reviews it: the turn is refused naming the person,
    /// before the self-review check. Falsified by reading the context type.
    #[tokio::test]
    async fn a_context_a_person_performs_runs_no_model_turns() {
        let d = crate::kj::test_helpers::test_dispatcher().await;
        let root = crate::kj::test_helpers::register_root_context(&d);
        let amy = crate::kj::test_helpers::test_reviewer_principal();
        let lane = crate::kj::test_helpers::register_context(&d, Some("amy-lane"), Some(root), amy);
        d.kernel_db().lock().assign_test_review(lane, Some(amy), None).unwrap();
        d.kernel_db().lock().update_context_type(lane, "coder").unwrap();
        for context in [root, lane] {
            let Err(IdentityError::Refused(message)) = resolve_for_context(d.kernel(), context).await else {
                panic!("a person's context must refuse a model turn");
            };
            assert!(message.contains("amy is a person") && message.contains("console"), "{message}");
            assert!(!message.contains("review"), "the person rule comes before the self-review check: {message}");
        }
    }

    /// A routing error the senpai's holder can repair refuses; corruption and
    /// every other failure is a fault.
    #[test]
    fn routing_errors_split_into_refusals_and_faults() {
        use crate::kernel_db::KernelDbError;
        let refused = |e| matches!(identity_error_from(e), IdentityError::Refused(_));
        assert!(refused(KernelDbError::Validation("nobody reviews".into())));
        assert!(refused(KernelDbError::NoReviewerAbove("nobody above".into())));
        assert!(!refused(KernelDbError::Corrupt("a senpai cycle".into())));
        assert!(!refused(KernelDbError::NotFound("a senpai with no performer".into())));
    }

    #[test]
    fn unknown_and_retired_characters_cannot_perform_or_review() {
        let db = KernelDb::temporary().unwrap();
        let coder = character(&db, "coder");
        let amy = character(&db, "amy");
        assert!(resolve(&db, Some(PrincipalId::new()), amy).unwrap_err().contains("no character sheet"));
        db.retire_character(coder, 1).unwrap();
        assert!(resolve(&db, Some(coder), amy).unwrap_err().contains("performer 'coder' is retired"));
    }
}

/// Why a model turn has no identity to run under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IdentityError {
    /// The context's assignment does not allow a model turn: the human
    /// root, no performer, a self-reviewing, retired, or unknown performer.
    /// Changing the assignment, or the context, is the remedy.
    Refused(String),
    /// The assignment could not be read or the reviewer walk failed.
    Failed(String),
}

impl From<IdentityError> for String {
    fn from(error: IdentityError) -> String {
        match error {
            IdentityError::Refused(message) | IdentityError::Failed(message) => message,
        }
    }
}

/// A routing error that the senpai's holder can repair (nobody above the
/// performer, a retired holder) refuses; stored state that contradicts an
/// invariant, and every other failure, is a fault.
fn identity_error_from(error: crate::kernel_db::KernelDbError) -> IdentityError {
    use crate::kernel_db::KernelDbError;
    match error {
        KernelDbError::Validation(why) | KernelDbError::NoReviewerAbove(why) => IdentityError::Refused(why),
        other => IdentityError::Failed(other.to_string()),
    }
}

/// Resolve the identity a model turn in `context_id` needs: its performer
/// and the reviewer chained above it. `prompt::submit` runs this before the
/// input commits and turn startup runs it again, so a refusal can come
/// before anything lands.
pub(crate) async fn resolve_for_context(
    kernel: &Kernel,
    context_id: ContextId,
) -> Result<(TurnIdentity, ResolvedContextReview), IdentityError> {
    let row = kernel.kernel_db().lock().get_context(context_id)
        .map_err(|e| IdentityError::Failed(format!("Could not read performer assignment: {e}")))?
        .ok_or_else(|| IdentityError::Failed(format!("No such context: {context_id}")))?;
    let actor = row.performer;
    let performer = require_performer(actor).map_err(IdentityError::Refused)?;
    let sheet = kernel.kernel_db().lock().get_character(performer)
        .map_err(|e| IdentityError::Failed(format!("Could not resolve performer: {e}")))?;
    let Some(sheet) = sheet else {
        return Err(IdentityError::Failed(format!(
            "The assigned performer {performer} has no character sheet, which stored state requires."
        )));
    };
    if sheet.root {
        return Err(IdentityError::Refused(person_refusal(&sheet.name)));
    }
    let routed = kernel.kernel_db().lock()
        .route_review(crate::kernel_db::ReviewQuery::ask(context_id, performer))
        .map_err(identity_error_from)?;
    let review = kernel.review_from_routed(routed).map_err(IdentityError::Failed)?;
    let db = kernel.kernel_db().lock();
    resolve(&db, actor, review.reviewer.principal_id)
        .map(|identity| (identity, review))
        .map_err(IdentityError::Refused)
}
