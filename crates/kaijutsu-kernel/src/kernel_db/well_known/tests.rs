use super::*;

/// Basic set → get → clear roundtrip.
#[test]
fn well_known_context_set_get_clear_roundtrip() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let row = make_context_row(Some("lost+found"));
    insert_context_with_doc(&db, &row, ws_id);

    assert_eq!(db.well_known_context(WellKnownRole::LostFound).unwrap(), None);

    db.set_well_known_context(WellKnownRole::LostFound, row.context_id).unwrap();
    assert_eq!(
        db.well_known_context(WellKnownRole::LostFound).unwrap(),
        Some(row.context_id)
    );

    db.clear_well_known_context(WellKnownRole::LostFound).unwrap();
    assert_eq!(db.well_known_context(WellKnownRole::LostFound).unwrap(), None);
}

/// This is the rotation primitive: assigning a role that is already held
/// UPDATEs rather than erroring, and the second id wins.
#[test]
fn set_well_known_context_twice_rotates_rather_than_conflicts() {
    let db = KernelDb::temporary().unwrap();
    let ws_id = setup_test_db(&db);
    let incumbent = make_context_row(Some("lost+found"));
    insert_context_with_doc(&db, &incumbent, ws_id);
    let successor = make_context_row(Some("lost+found-2"));
    insert_context_with_doc(&db, &successor, ws_id);

    db.set_well_known_context(WellKnownRole::LostFound, incumbent.context_id).unwrap();
    assert_eq!(
        db.well_known_context(WellKnownRole::LostFound).unwrap(),
        Some(incumbent.context_id)
    );

    // Re-assigning must not error, and the new id must win.
    db.set_well_known_context(WellKnownRole::LostFound, successor.context_id).unwrap();
    assert_eq!(
        db.well_known_context(WellKnownRole::LostFound).unwrap(),
        Some(successor.context_id)
    );

    // The incumbent's own row is untouched — rotation reassigns the
    // registry pointer, it does not delete or rename anything.
    let incumbent_row = db.find_context_by_label("lost+found").unwrap().unwrap();
    assert_eq!(incumbent_row.context_id, incumbent.context_id);
}
