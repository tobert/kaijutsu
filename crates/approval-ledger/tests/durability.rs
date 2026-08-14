//! Guarantee 1: durable before asked.
//!
//! `create_ask` must commit to disk before it returns. This test opens a
//! real on-disk database (an in-memory one proves nothing here — it has
//! no persistence to lose in the first place), calls `create_ask`, then
//! drops the connection entirely — standing in for a process crash
//! immediately after `create_ask` returns and before any prompt is ever
//! shown — and opens a **new** connection to the same file to check the
//! row is there.

use approval_ledger::types::{ApprovalStatus, NewAsk, NewOption, Origin};
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

#[test]
fn a_created_ask_survives_the_connection_that_created_it_being_dropped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("ledger.sqlite3");

    let request_id = {
        let conn = Connection::open(&db_path).expect("open db");
        approval_ledger::migrate(&conn).expect("migrate");
        let id = approval_ledger::ask::create_ask(&conn, &minimal_ask()).expect("create_ask");
        // `conn` drops here — standing in for the process crashing right
        // after `create_ask` returned, before any prompt was shown. If
        // `create_ask` had not already committed, this is exactly where
        // the row would be lost.
        id
    };

    let conn2 = Connection::open(&db_path).expect("reopen db");
    let row = approval_ledger::ask::get_approval(&conn2, &request_id)
        .expect("query")
        .expect("the row must have survived the dropped connection");
    assert_eq!(row.status, ApprovalStatus::Pending);
}

#[test]
fn a_full_statement_tree_also_survives_the_dropped_connection() {
    use approval_ledger::types::{NewPlanCommand, NewPlanStatement, NewPlanVar, NewPlannedValue, VarBinding};

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("ledger.sqlite3");

    let (request_id, digest) = {
        let conn = Connection::open(&db_path).expect("open db");
        approval_ledger::migrate(&conn).expect("migrate");
        let mut ask = minimal_ask();
        let digest = "durability-digest".to_string();
        ask.statements = vec![NewPlanStatement {
            statement_digest: digest.clone(),
            rendered: "rm ${TARGET}".into(),
            statement_kind: "command".into(),
            commands: vec![NewPlanCommand {
                name: "rm".into(),
                args: vec![NewPlannedValue::Plain("${TARGET}".into())],
                redirects: vec![],
                backgrounded: false,
            }],
            vars: vec![NewPlanVar { name: "TARGET".into(), binding: VarBinding::Free }],
        }];
        let id = approval_ledger::ask::create_ask(&conn, &ask).expect("create_ask");
        (id, digest)
    };

    let conn2 = Connection::open(&db_path).expect("reopen db");
    let linked = approval_ledger::ask::load_ask_statements(&conn2, &request_id).unwrap();
    assert_eq!(linked.len(), 1);
    assert_eq!(linked[0].statement.statement_digest, digest);
    assert_eq!(linked[0].statement.free_vars, vec!["TARGET".to_string()]);

    // Directly by digest, too — the content-addressed body is its own
    // durable fact, independent of any one ask that references it.
    let statement = approval_ledger::ask::load_statement(&conn2, &digest).unwrap().unwrap();
    assert_eq!(statement.free_vars, vec!["TARGET".to_string()]);
}
