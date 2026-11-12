use super::*;

fn file(path: &str, visited: bool, line: usize, col: usize) -> RecoveryFile {
    RecoveryFile { path: path.to_string(), visited, cursor_line: line, cursor_col: col }
}

#[test]
fn a_record_round_trips_with_its_files_in_list_order() {
    let db = KernelDb::temporary().unwrap();
    let (id, who, ctx) = (RecoveryId::new(), PrincipalId::new(), ContextId::new());
    let files = [file("/a.rs", true, 3, 4), file("/b.rs", false, 0, 0), file("/c.rs", true, 9, 1)];
    db.write_editor_recovery(id, who, ctx, 2, &files).unwrap();

    let got = db.get_editor_recovery(id).unwrap().expect("record exists");
    assert_eq!(got.recovery_id, id);
    assert_eq!((got.opener_principal, got.opener_context_id, got.current_index), (who, ctx, 2));
    assert_eq!(got.files, files);
    assert!(got.opened_at > 0 && got.updated_at >= got.opened_at);
}

#[test]
fn a_second_write_replaces_the_files_and_keeps_opened_at() {
    let db = KernelDb::temporary().unwrap();
    let (id, who, ctx) = (RecoveryId::new(), PrincipalId::new(), ContextId::new());
    db.write_editor_recovery(id, who, ctx, 0, &[file("/a", true, 0, 0), file("/b", false, 0, 0)]).unwrap();
    let first = db.get_editor_recovery(id).unwrap().unwrap();

    std::thread::sleep(std::time::Duration::from_millis(3));
    db.write_editor_recovery(id, who, ctx, 0, &[file("/a", true, 5, 6)]).unwrap();
    let second = db.get_editor_recovery(id).unwrap().unwrap();

    assert_eq!(second.files, [file("/a", true, 5, 6)], "a shorter list leaves no stale tail row");
    assert_eq!(second.opened_at, first.opened_at);
    assert!(second.updated_at > first.updated_at);
    assert_eq!(db.list_editor_recoveries().unwrap().len(), 1, "one record, not two");
}

#[test]
fn delete_removes_the_record_and_cascades_to_its_files() {
    let db = KernelDb::temporary().unwrap();
    let (id, who, ctx) = (RecoveryId::new(), PrincipalId::new(), ContextId::new());
    db.write_editor_recovery(id, who, ctx, 0, &[file("/a", true, 0, 0), file("/b", false, 0, 0)]).unwrap();

    assert!(db.delete_editor_recovery(id).unwrap());
    assert!(db.get_editor_recovery(id).unwrap().is_none());
    assert!(!db.delete_editor_recovery(id).unwrap(), "a second delete finds nothing");
    let orphans: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM editor_recovery_files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(orphans, 0, "the foreign key cascade removed the file rows");
}

#[test]
fn list_is_newest_write_first() {
    let db = KernelDb::temporary().unwrap();
    let (who, ctx) = (PrincipalId::new(), ContextId::new());
    let (old, new) = (RecoveryId::new(), RecoveryId::new());
    db.write_editor_recovery(old, who, ctx, 0, &[file("/old", true, 0, 0)]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(3));
    db.write_editor_recovery(new, who, ctx, 0, &[file("/new", true, 0, 0)]).unwrap();

    let ids: Vec<_> = db.list_editor_recoveries().unwrap().into_iter().map(|s| s.recovery_id).collect();
    assert_eq!(ids, [new, old]);

    std::thread::sleep(std::time::Duration::from_millis(3));
    db.write_editor_recovery(old, who, ctx, 0, &[file("/old", true, 1, 1)]).unwrap();
    let ids: Vec<_> = db.list_editor_recoveries().unwrap().into_iter().map(|s| s.recovery_id).collect();
    assert_eq!(ids, [old, new], "a later write moves the record up");
}

#[test]
fn an_empty_list_or_an_index_past_the_end_is_refused_and_leaves_the_record_alone() {
    let db = KernelDb::temporary().unwrap();
    let (id, who, ctx) = (RecoveryId::new(), PrincipalId::new(), ContextId::new());
    assert!(db.write_editor_recovery(id, who, ctx, 0, &[]).is_err());
    assert!(db.write_editor_recovery(id, who, ctx, 1, &[file("/a", true, 0, 0)]).is_err());
    assert!(db.get_editor_recovery(id).unwrap().is_none(), "a refused write stores nothing");

    db.write_editor_recovery(id, who, ctx, 0, &[file("/a", true, 7, 7)]).unwrap();
    assert!(db.write_editor_recovery(id, who, ctx, 3, &[file("/b", true, 0, 0)]).is_err());
    assert_eq!(db.get_editor_recovery(id).unwrap().unwrap().files, [file("/a", true, 7, 7)]);
}

#[test]
fn a_file_row_cannot_exist_without_its_session() {
    let db = KernelDb::temporary().unwrap();
    let orphan = db.conn.execute(
        "INSERT INTO editor_recovery_files (recovery_id, position, path, visited) VALUES (?1, 0, '/a', 1)",
        params![RecoveryId::new().as_bytes().as_slice()],
    );
    assert!(orphan.is_err(), "the foreign key refuses a file with no session");
}

#[test]
fn matches_short_takes_a_tail_prefix_or_the_full_id_and_never_the_clock_front() {
    let id = RecoveryId::new();
    assert!(id.matches_short(&id.short()));
    assert!(id.matches_short(&id.short()[..3]));
    assert!(id.matches_short(&id.to_hex()));
    assert!(!id.matches_short(""), "an empty pick names nothing");
    assert!(!id.matches_short(&id.to_hex()[..8]), "the leading hex is a clock, not a handle");
}

#[test]
fn a_session_row_with_no_files_or_a_bad_index_is_an_error_naming_the_record() {
    let db = KernelDb::temporary().unwrap();
    let id = RecoveryId::new();
    db.conn
        .execute(
            "INSERT INTO editor_recovery_sessions
                (recovery_id, opener_principal, opener_context_id, current_index, opened_at, updated_at)
             VALUES (?1, ?2, ?3, 0, 1, 1)",
            params![id.as_bytes().as_slice(), PrincipalId::new().as_bytes().as_slice(), ContextId::new().as_bytes().as_slice()],
        )
        .unwrap();
    for result in [db.get_editor_recovery(id).map(|_| ()), db.list_editor_recoveries().map(|_| ())] {
        let err = result.unwrap_err().to_string();
        assert!(err.contains(&id.short()) && err.contains("no files"), "{err}");
    }

    db.conn
        .execute(
            "INSERT INTO editor_recovery_files (recovery_id, position, path, visited) VALUES (?1, 0, '/a', 1)",
            params![id.as_bytes().as_slice()],
        )
        .unwrap();
    assert!(db.get_editor_recovery(id).unwrap().is_some());
    db.conn
        .execute("UPDATE editor_recovery_sessions SET current_index = 4", [])
        .unwrap();
    let err = db.get_editor_recovery(id).unwrap_err().to_string();
    assert!(err.contains(&id.short()) && err.contains("index 4"), "{err}");
}

#[test]
fn the_entry_listing_keeps_good_records_beside_a_corrupt_one() {
    let db = KernelDb::temporary().unwrap();
    let good = RecoveryId::new();
    db.write_editor_recovery(
        good,
        PrincipalId::new(),
        ContextId::new(),
        0,
        &[RecoveryFile { path: "/a".into(), visited: true, cursor_line: 0, cursor_col: 0 }],
    )
    .unwrap();
    let bad = RecoveryId::new();
    db.conn
        .execute(
            "INSERT INTO editor_recovery_sessions
                (recovery_id, opener_principal, opener_context_id, current_index, opened_at, updated_at)
             VALUES (?1, ?2, ?3, 0, 1, 1)",
            params![bad.as_bytes().as_slice(), PrincipalId::new().as_bytes().as_slice(), ContextId::new().as_bytes().as_slice()],
        )
        .unwrap();
    assert!(db.list_editor_recoveries().is_err(), "the strict listing still fails loudly");
    let entries = db.list_editor_recovery_entries().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(matches!(&entries[0], RecoveryEntry::Valid(s) if s.recovery_id == good));
    assert!(matches!(&entries[1], RecoveryEntry::Corrupt(c) if c.recovery_id == bad && c.reason == "no files"));
}
