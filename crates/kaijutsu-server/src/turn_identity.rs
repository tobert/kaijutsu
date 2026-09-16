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
    if character.root {
        return Err(format!("A root character has no model, so '{}' cannot perform a turn. Cast a character a model plays with 'kj context set . --as <character>'.", character.name));
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

    /// A root character has no model: turn identity refuses it as a
    /// performer, naming the rule, before any provider call.
    #[test]
    fn a_root_character_cannot_perform_a_turn() {
        let db = KernelDb::temporary().unwrap();
        let amy = character(&db, "amy");
        let banto = character(&db, "banto");
        db.update_character_root(amy, true).unwrap();
        let error = resolve(&db, Some(amy), banto).unwrap_err();
        assert!(error.contains("root character has no model"), "{error}");
        assert!(error.contains("amy"), "{error}");
        // The same character, no longer a root, performs normally.
        db.update_character_root(amy, false).unwrap();
        assert!(resolve(&db, Some(amy), banto).is_ok());
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
