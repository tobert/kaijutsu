use super::*;

#[test]
fn quiesce_flag_defaults_clear_and_roundtrips() {
    let db = KernelDb::temporary().unwrap();

    // A fresh kernel runs. Absence of the row IS the running state, so
    // this also proves the schema does not seed one.
    assert!(db.quiesce_state().unwrap().is_none());

    let who = PrincipalId::new();
    db.set_quiesced(who, Some("wake storm")).unwrap();
    let state = db.quiesce_state().unwrap().expect("quiesced");
    assert_eq!(state.quiesced_by, who);
    assert_eq!(state.reason.as_deref(), Some("wake storm"));
    assert!(state.since_unix_ms > 0);

    // Re-quiescing updates rather than erroring on the singleton PK, and
    // the newest decision is the one that shows — but the ORIGINAL stop
    // time survives. Correcting a reason must not erase how long the
    // kernel has been down.
    let first_since = state.since_unix_ms;
    let who2 = PrincipalId::new();
    db.set_quiesced(who2, None).unwrap();
    let state = db.quiesce_state().unwrap().expect("still quiesced");
    assert_eq!(state.quiesced_by, who2);
    assert_eq!(state.reason, None);
    assert_eq!(state.since_unix_ms, first_since, "re-quiesce reset the clock");

    // A full resume/quiesce cycle DOES start a new clock — this is a new
    // stop, not a correction of the old one.
    assert!(db.clear_quiesced().unwrap());
    db.set_quiesced(who2, None).unwrap();
    let restarted = db.quiesce_state().unwrap().expect("quiesced again");
    assert!(restarted.since_unix_ms >= first_since);

    // clear() reports whether it changed anything, so `resume` can tell
    // "resumed" from "was already running".
    assert!(db.clear_quiesced().unwrap());
    assert!(db.quiesce_state().unwrap().is_none());
    assert!(!db.clear_quiesced().unwrap());
}

#[test]
fn quiesce_flag_survives_reopen() {
    // The whole reason the flag is durable: a kernel that came back
    // running is not an emergency stop. Restart is simulated by dropping
    // the connection and opening the same file again.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kernel.db");
    let who = PrincipalId::new();

    {
        let db = KernelDb::open(&path).unwrap();
        db.set_quiesced(who, Some("seppuku")).unwrap();
    }

    let db = KernelDb::open(&path).unwrap();
    let state = db.quiesce_state().unwrap().expect("still quiesced after reopen");
    assert_eq!(state.quiesced_by, who);
    assert_eq!(state.reason.as_deref(), Some("seppuku"));

    // And resume is durable in the same direction.
    assert!(db.clear_quiesced().unwrap());
    drop(db);
    let db = KernelDb::open(&path).unwrap();
    assert!(db.quiesce_state().unwrap().is_none());
}

#[test]
fn quiesce_read_failure_is_an_error_not_a_running_kernel() {
    // The turn path refuses when this read fails, and that decision only
    // makes sense if the failure is distinguishable from "not quiesced".
    // A read that degraded to None would start the turn an operator was
    // stopping — the one outcome the flag exists to prevent.
    let db = KernelDb::temporary().unwrap();
    assert!(db.quiesce_state().unwrap().is_none());

    db.poison_quiesce_table_for_test().unwrap();
    assert!(
        db.quiesce_state().is_err(),
        "a failed quiesce read must error, never report a running kernel"
    );
}

#[test]
fn get_or_create_kernel_id_stable_across_calls() {
    let db = KernelDb::temporary().unwrap();
    let id1 = db.kernel_id().unwrap();
    let id2 = db.kernel_id().unwrap();
    assert_eq!(id1, id2, "should return same ID on second call");
}

#[test]
fn get_or_create_kernel_id_fresh_on_empty_db() {
    let db = KernelDb::temporary().unwrap();
    let id = db.kernel_id().unwrap();
    // Should be a valid UUIDv7 (non-zero)
    assert_ne!(id.as_bytes(), &[0u8; 16]);
}
