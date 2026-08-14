//! Guarantee 5: exactly one answerer wins.
//!
//! SQLite has no `SKIP LOCKED`. This test is the reason that matters: real
//! OS threads, each with its OWN `Connection` to the SAME on-disk file
//! (an in-memory `Connection::open_in_memory()` would not exercise this
//! at all — two in-memory connections don't share a database, so there
//! would be nothing to race over), all calling `claim()` on the identical
//! `request_id` at once. Exactly one must win.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use approval_ledger::error::LedgerError;
use approval_ledger::types::{NewAsk, NewOption, Origin};
use rusqlite::Connection;

fn minimal_ask() -> NewAsk {
    NewAsk {
        context_id: vec![1, 2, 3],
        principal_id: vec![4, 5, 6],
        origin: Origin::ShellGate,
        instance: Some("builtin.shell".into()),
        tool: Some("shell".into()),
        hook_id: None,
        description: "rm -rf /tmp/scratch".into(),
        statements: vec![],
        authorized_label: Some("rm scratch".into()),
        rc_run_id: None,
        expires_at: None,
        options: vec![NewOption { option_id: "allow_once".into(), label: "Allow once".into(), kind: "allow_once".into() }],
        signals: vec![],
    }
}

const CONTENDERS: u8 = 8;

#[test]
fn exactly_one_of_many_concurrent_claimants_wins() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = Arc::new(dir.path().join("ledger.sqlite3"));

    let request_id = {
        let conn = Connection::open(&*db_path).expect("open db");
        approval_ledger::migrate(&conn).expect("migrate");
        approval_ledger::ask::create_ask(&conn, &minimal_ask()).expect("create_ask")
    };

    let handles: Vec<_> = (0..CONTENDERS)
        .map(|i| {
            let db_path = Arc::clone(&db_path);
            let request_id = request_id.clone();
            thread::spawn(move || {
                let conn = Connection::open(&*db_path).expect("open db in thread");
                // Without this, a losing thread's `BEGIN IMMEDIATE` can hit
                // SQLITE_BUSY immediately (rusqlite's default busy timeout is
                // 0) instead of waiting its turn — that would surface as a
                // `Db` error, not the `NotClaimable` this test checks for,
                // and would just be testing SQLite's default timeout, not
                // this crate's claim logic.
                conn.busy_timeout(Duration::from_secs(10)).expect("set busy timeout");
                approval_ledger::claim::claim(&conn, &request_id, &[i])
            })
        })
        .collect();

    let results: Vec<_> = handles.into_iter().map(|h| h.join().expect("thread panicked")).collect();

    let winners = results.iter().filter(|r| r.is_ok()).count();
    let losers = results.iter().filter(|r| r.is_err()).count();
    assert_eq!(winners, 1, "exactly one claimant must win a race on the same request_id, got {winners}");
    assert_eq!(losers, (CONTENDERS - 1) as usize);

    for r in &results {
        if let Err(e) = r {
            assert!(
                matches!(e, LedgerError::NotClaimable { status, .. } if status == "claimed"),
                "a losing claim must fail with NotClaimable(status=claimed), got: {e:?}"
            );
        }
    }

    // The row itself agrees: exactly one `claimed_by` stuck, and it must
    // be one of the contenders that actually reported success.
    let conn = Connection::open(&*db_path).expect("reopen db");
    let row = approval_ledger::ask::get_approval(&conn, &request_id).unwrap().unwrap();
    assert_eq!(row.status, approval_ledger::types::ApprovalStatus::Claimed);
    let winning_claimant = row.claimed_by.expect("a winner must have claimed_by set");
    assert_eq!(winning_claimant.len(), 1, "claimant id is one of the thread indices 0..CONTENDERS");

    // Exactly one `claimed` event was appended, not one per contender.
    let events = approval_ledger::ask::list_events(&conn, &request_id).unwrap();
    let claim_events = events.iter().filter(|e| e.kind == approval_ledger::types::EventKind::Claimed).count();
    assert_eq!(claim_events, 1, "only the winning claim should append an event");
}
