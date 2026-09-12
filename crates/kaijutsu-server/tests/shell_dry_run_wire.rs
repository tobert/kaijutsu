//! e2e: `shellDryRun` over the wire. The kernel evaluates its PreCall hooks
//! against a command it will never run, hands back what they would have
//! decided, and leaves the ledger with a record nobody has to answer.
//!
//! `docs/gate-and-shell-split.md`, "Dry-run mode".
//!
//! The hooks are pushed onto the live broker's table rather than installed
//! through `kj hook add`: the subject here is the RPC method and what it
//! leaves in the ledger, and a test context's loadout does not carry the
//! config-write authority `kj hook add` requires.

mod common;

use common::{connect_client, run_local, start_server_with_kernel_handle};
use kaijutsu_client::ShellDryRunOutcome;
use kaijutsu_kernel::kernel_db::CharacterRow;
use kaijutsu_kernel::ApprovalStatus;
use kaijutsu_kernel::mcp::{AskSpec, GlobPattern, HookAction, HookEntry, HookId};
use kaijutsu_server::SharedKernel;
use kaijutsu_types::{AskStatus, PrincipalId};

/// Push one PreCall hook matched on `shell_write` onto the live broker.
async fn install_hook(kernel: &SharedKernel, id: &str, action: HookAction) {
    kernel
        .kernel
        .broker()
        .hooks()
        .write()
        .await
        .pre_call
        .entries
        .push(HookEntry {
            id: HookId(id.to_string()),
            match_instance: None,
            match_tool: Some(GlobPattern("shell_write".into())),
            match_context: None,
            match_principal: None,
            action,
            priority: 0,
            kaish_script_id: None,
        });
}

/// The headline: a denying hook produces a would-deny report carrying the
/// hook's own id and reason, the ledger holds the abandoned row the report
/// names, and nothing is left pending for a human.
///
/// Falsified by enforcing the verdict instead of reporting it (the call
/// fails rather than returning a report), and by leaving the recorded row
/// pending (the `list_pending_asks` assertion trips).
#[test]
fn a_denying_hook_reports_a_would_deny_and_asks_nobody() {
    run_local(async {
        let (addr, kernel) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _kernel_id) = client.bind_kernel().await.unwrap();
        let context_id = kj.create_context("dry-run-deny-wire").await.unwrap();

        install_hook(
            &kernel,
            "wire-deny",
            HookAction::Deny("the wire test says no".into()),
        )
        .await;

        let report = kj
            .shell_dry_run(context_id, "git push --force origin main")
            .await
            .expect("a dry run reports; it does not refuse");

        assert_eq!(report.outcome, ShellDryRunOutcome::WouldDeny);
        assert_eq!(report.hook_id.as_deref(), Some("wire-deny"));
        let reason = report.reason.expect("a would-deny carries a reason");
        assert!(
            reason.contains("the wire test says no"),
            "the hook's own reason must reach the caller, got {reason}"
        );

        let ask = report.ask.expect("a would-deny is recorded durably");
        assert_eq!(
            ask.status,
            AskStatus::Abandoned,
            "a dry-run row records a question, never an answer"
        );

        let db = kernel.kernel_db.lock();
        let row = db
            .get_approval(&ask.request_id)
            .unwrap()
            .expect("the report names a row that is really in the ledger");
        assert_eq!(row.status, ApprovalStatus::Abandoned);
        assert!(
            db.list_pending_asks().unwrap().is_empty(),
            "a dry run must leave nothing for a human to answer"
        );
    });
}

/// The invariant the whole mode rests on: a hook that WOULD open a gate
/// opens none. The report says a human would have been asked, the row is
/// there to count later, and `kj ledger list` stays empty.
#[test]
fn an_asking_hook_reports_a_would_ask_and_leaves_no_pending_ask() {
    run_local(async {
        let (addr, kernel) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let actor = client.whoami().await.unwrap().principal_id;
        let (kj, _kernel_id) = client.bind_kernel().await.unwrap();
        let context_id = kj.create_context("dry-run-ask-wire").await.unwrap();
        let reviewer = PrincipalId::new();
        {
            let db = kernel.kernel_db.lock();
            db.insert_character(&CharacterRow {
                principal_id: reviewer,
                name: "dry-run-reviewer".into(),
                created_at: 0,
                retired_at: None,
                handoff_ctx: None,
            })
            .unwrap();
            db.update_context_review(context_id, Some(actor), Some(reviewer))
                .unwrap();
        }

        install_hook(
            &kernel,
            "wire-ask",
            HookAction::Ask(AskSpec { description: Some("scored risky".into()) }),
        )
        .await;

        let report = kj
            .shell_dry_run(context_id, "echo dry-run-hook-ask")
            .await
            .expect("a dry run reports; it does not refuse");

        assert_eq!(report.outcome, ShellDryRunOutcome::WouldAsk);
        assert_eq!(report.hook_id.as_deref(), Some("wire-ask"));
        let ask = report.ask.expect("a would-ask is recorded durably");
        assert_eq!(ask.status, AskStatus::Abandoned);

        let db = kernel.kernel_db.lock();
        assert!(
            db.list_pending_asks().unwrap().is_empty(),
            "nobody may be asked about a command the kernel is not running"
        );
        let row = db.get_approval(&ask.request_id).unwrap().unwrap();
        assert_eq!(row.status, ApprovalStatus::Abandoned);
        assert!(
            row.description.contains("scored risky"),
            "the hook's description rides the recorded row, got {}",
            row.description
        );
    });
}

/// Non-vacuity for both tests above: with no hook installed the same call
/// reports a would-proceed and records nothing at all, so a report that
/// says "would deny" is saying something.
#[test]
fn no_matching_hook_reports_a_would_proceed_and_records_nothing() {
    run_local(async {
        let (addr, kernel) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _kernel_id) = client.bind_kernel().await.unwrap();
        let context_id = kj.create_context("dry-run-clean-wire").await.unwrap();

        let report = kj
            .shell_dry_run(context_id, "echo hello")
            .await
            .expect("a dry run reports; it does not refuse");

        assert_eq!(report.outcome, ShellDryRunOutcome::WouldProceed);
        assert_eq!(report.hook_id, None);
        assert_eq!(report.reason, None);
        assert_eq!(report.ask, None);
        assert!(
            kernel.kernel_db.lock().list_pending_asks().unwrap().is_empty(),
            "and nothing is waiting on anyone"
        );
    });
}
