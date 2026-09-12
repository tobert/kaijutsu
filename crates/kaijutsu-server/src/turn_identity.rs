//! Resolve the performer and reviewer before starting model work.

use kaijutsu_kernel::kernel_db::KernelDb;
use kaijutsu_types::PrincipalId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TurnIdentity {
    pub actor: PrincipalId,
    pub reviewer: PrincipalId,
}

pub(crate) fn resolve(
    db: &KernelDb,
    actor: Option<PrincipalId>,
    reviewer: PrincipalId,
) -> Result<TurnIdentity, String> {
    let actor = actor.ok_or_else(||
        "No performer assigned. Use 'kj context set . --as <character>' before starting a model turn.".to_string()
    )?;
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
    use kaijutsu_kernel::kernel_db::CharacterRow;

    fn character(db: &KernelDb, name: &str) -> PrincipalId {
        let principal_id = PrincipalId::new();
        db.insert_character(&CharacterRow {
            principal_id, name: name.into(), created_at: 0,
            retired_at: None, handoff_ctx: None,
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
