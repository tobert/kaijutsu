use super::*;

/// Create, fetch by name, fetch by principal, list, and retire — the
/// full lifecycle a sheet row goes through in this slice.
#[test]
fn character_crud_roundtrip() {
    let db = KernelDb::temporary().unwrap();
    let principal = PrincipalId::new();
    db.insert_character(&CharacterRow { context_type: None,
        principal_id: principal,
        name: "hajime".to_string(),
        created_at: 1000,
        retired_at: None,
        handoff_ctx: None, root_ctx: None, root: false,
    })
    .unwrap();

    let by_name = db.get_character_by_name("hajime").unwrap().unwrap();
    assert_eq!(by_name.principal_id, principal);
    let by_principal = db.get_character(principal).unwrap().unwrap();
    assert_eq!(by_principal.name, "hajime");

    assert!(db.get_character_by_name("nobody").unwrap().is_none());
    assert!(db.get_character(PrincipalId::new()).unwrap().is_none());

    let live = db.list_characters(false).unwrap();
    assert_eq!(live.len(), 1);

    // Retire: first call newly retires, second is a no-op that still
    // reports "nothing new happened" rather than an error.
    assert!(db.retire_character(principal, 2000).unwrap());
    assert!(!db.retire_character(principal, 3000).unwrap());
    let retired = db.get_character(principal).unwrap().unwrap();
    assert_eq!(retired.retired_at, Some(2000), "first retire wins the timestamp");

    // A retired character drops out of the live-only listing but stays
    // reachable with `include_retired`.
    assert!(db.list_characters(false).unwrap().is_empty());
    assert_eq!(db.list_characters(true).unwrap().len(), 1);
}

/// A duplicate name fails loudly rather than minting a second principal
/// under the same name.
#[test]
fn character_duplicate_name_is_rejected() {
    let db = KernelDb::temporary().unwrap();
    db.insert_character(&CharacterRow { context_type: None,
        principal_id: PrincipalId::new(),
        name: "hajime".to_string(),
        created_at: 1000,
        retired_at: None,
        handoff_ctx: None, root_ctx: None, root: false,
    })
    .unwrap();
    let err = db
        .insert_character(&CharacterRow { context_type: None,
            principal_id: PrincipalId::new(),
            name: "hajime".to_string(),
            created_at: 2000,
            retired_at: None,
            handoff_ctx: None, root_ctx: None, root: false,
        })
        .unwrap_err();
    assert!(matches!(err, KernelDbError::LabelConflict(_)), "got: {err}");
}

/// `root` round-trips through insert and through
/// `update_character_root`, in both directions.
#[test]
fn character_root_flag_round_trips_in_both_directions() {
    let db = KernelDb::temporary().unwrap();
    let amy = PrincipalId::new();
    let coder = PrincipalId::new();
    db.insert_character(&CharacterRow { context_type: None,
        principal_id: amy, name: "amy".into(), created_at: 0,
        retired_at: None, handoff_ctx: None, root_ctx: None, root: true,
    }).unwrap();
    db.insert_character(&CharacterRow { context_type: None,
        principal_id: coder, name: "coder".into(), created_at: 0,
        retired_at: None, handoff_ctx: None, root_ctx: None, root: false,
    }).unwrap();
    assert!(db.get_character(amy).unwrap().unwrap().root, "a root inserts as a root");
    assert!(!db.get_character(coder).unwrap().unwrap().root);

    db.update_character_root(coder, true).unwrap();
    assert!(db.get_character(coder).unwrap().unwrap().root);
    db.update_character_root(coder, false).unwrap();
    assert!(!db.get_character(coder).unwrap().unwrap().root);

    let error = db.update_character_root(PrincipalId::new(), true).unwrap_err();
    assert!(matches!(error, KernelDbError::NotFound(_)), "{error}");
}

/// `contexts_played_by` returns only the LIVE contexts a principal
/// plays — an archived one drops out, matching what `kj character
/// retire` must NOT try to archive twice.
#[test]
fn contexts_played_by_excludes_archived() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let character = PrincipalId::new();

    let live = make_context_row(Some("live-ctx"));
    insert_context_with_doc(&db, &live, ws_id);
    db.update_performer(live.context_id, Some(character)).unwrap();

    let archived = make_context_row(Some("archived-ctx"));
    insert_context_with_doc(&db, &archived, ws_id);
    db.update_performer(archived.context_id, Some(character)).unwrap();
    db.archive_context(archived.context_id).unwrap();

    let other = make_context_row(Some("other-ctx"));
    insert_context_with_doc(&db, &other, ws_id);
    db.update_performer(other.context_id, Some(PrincipalId::new())).unwrap();

    let played = db.contexts_played_by(character).unwrap();
    assert_eq!(played.len(), 1);
    assert_eq!(played[0].context_id, live.context_id);
}

/// `name_for` checks the two sentinels first, then the sheet, and
/// renders an unmapped principal as its short id rather than blank.
#[test]
fn name_for_resolves_sentinels_then_sheet_then_short_id() {
    let db = KernelDb::temporary().unwrap();
    assert_eq!(db.name_for(PrincipalId::system()), "system");
    assert_eq!(db.name_for(PrincipalId::beat()), "beat");

    let character = PrincipalId::new();
    db.insert_character(&CharacterRow { context_type: None,
        principal_id: character,
        name: "hajime".to_string(),
        created_at: 1000,
        retired_at: None,
        handoff_ctx: None, root_ctx: None, root: false,
    })
    .unwrap();
    assert_eq!(db.name_for(character), "hajime");

    let stranger = PrincipalId::new();
    assert_eq!(
        db.name_for(stranger),
        stranger.short(),
        "a credential with no characters row renders as its short id, never blank"
    );
}
