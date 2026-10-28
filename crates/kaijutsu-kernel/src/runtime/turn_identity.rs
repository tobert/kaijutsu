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

/// The human root (`docs/character.md`, "The root console and its shell")
/// has no model, so it is refused before its performer, who is its own
/// reviewer, meets the self-review refusal.
pub(crate) const HUMAN_ROOT_REFUSAL: &str = "The human root runs no model turns. Talk to a model in the ROOT console, or press Ctrl+Z for this root's shell.";

/// The performer is checked before the reviewer resolves: the reviewer walk
/// starts above the performer, so with none set it fails with a message
/// about the reviewer, which is not the repair the caller needs.
pub(crate) fn require_performer(actor: Option<PrincipalId>) -> Result<PrincipalId, String> {
    actor.ok_or_else(||
        "No performer assigned. Use 'kj context set . --as <character>' before starting a model turn.".to_string()
    )
}

pub(crate) fn resolve(
    db: &KernelDb,
    actor: Option<PrincipalId>,
    reviewer: PrincipalId,
) -> Result<TurnIdentity, String> {
    let actor = require_performer(actor)?;
    if actor == reviewer {
        return Err("The performer cannot review its own work. Ask the default review authority to assign a different reviewer with 'kj context set . --reviewer <character>'.".to_string());
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
        db.insert_character(&CharacterRow {
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

    /// A root character is a model context like any other: turn identity
    /// admits it as a performer when a distinct reviewer is assigned. The
    /// self-review refusal still holds for the root, so a root performer
    /// cannot review its own work.
    #[test]
    fn a_root_character_performs_a_turn_with_a_distinct_reviewer() {
        let db = KernelDb::temporary().unwrap();
        let amy = character(&db, "amy");
        let banto = character(&db, "banto");
        db.update_character_root(amy, true).unwrap();
        assert_eq!(resolve(&db, Some(amy), banto).unwrap(), TurnIdentity { actor: amy, reviewer: banto });
        let error = resolve(&db, Some(amy), amy).unwrap_err();
        assert!(error.contains("cannot review"), "{error}");
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
    if row.context_type == kaijutsu_types::ROOT_CONTEXT_TYPE {
        return Err(IdentityError::Refused(HUMAN_ROOT_REFUSAL.to_string()));
    }
    let actor = row.played_by;
    require_performer(actor).map_err(IdentityError::Refused)?;
    let review = kernel.resolve_context_review(context_id).await.map_err(IdentityError::Failed)?;
    let db = kernel.kernel_db().lock();
    resolve(&db, actor, review.reviewer.principal_id)
        .map(|identity| (identity, review))
        .map_err(IdentityError::Refused)
}
