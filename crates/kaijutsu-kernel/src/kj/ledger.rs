//! `kj ledger` — answer the approval ledger's pending asks.
//!
//! The answering half of the gate in [`crate::kj::gate`]: a gated verb
//! (`kj cc send` today) leaves a durable ask row and waits; a human — in
//! any shell, any client, minutes later if need be — answers it here.
//!
//! ```sh
//! kj ledger list               # what is waiting for a decision
//! kj ledger show <request-id>  # one ask, with its statement
//! kj ledger allow <request-id> # claim + allow (exactly one answerer wins)
//! kj ledger deny <request-id>  # claim + deny
//! ```
//!
//! Answering is a two-step ledger transaction by design: [`claim`] moves
//! the row `pending → claimed` under `BEGIN IMMEDIATE`, so concurrent
//! answerers race safely (guarantee 5 — exactly one wins; losers read a
//! loud `NotClaimable`, never a silent no-op), and [`decide`] makes the
//! terminal write. A row that went terminal elsewhere first answers back
//! `AlreadyDecided` — the late answer is still recorded in the event log,
//! but it does not overwrite anything (guarantee 6).

use approval_ledger::error::LedgerError;
use clap::{Parser, Subcommand};
use kaijutsu_types::{ContentType, ContextId};

use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};

#[derive(Parser, Debug)]
#[command(
    name = "ledger",
    about = "Answer pending approval-ledger asks left by gated kj verbs",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct LedgerArgs {
    #[command(subcommand)]
    command: LedgerCommand,
}

#[derive(Subcommand, Debug)]
enum LedgerCommand {
    /// List asks still waiting for a decision (status pending or claimed).
    List,
    /// Show one ask in full, including the statement being authorized.
    Show { request_id: String },
    /// Allow one ask (claims it first; exactly one answerer wins).
    Allow { request_id: String },
    /// Deny one ask (claims it first; exactly one answerer wins).
    Deny { request_id: String },
}

impl KjDispatcher {
    pub(crate) fn dispatch_ledger(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<LedgerArgs>();
        }
        let parsed = match LedgerArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj ledger: {e}"));
            }
        };
        match parsed.command {
            LedgerCommand::List => self.ledger_list(),
            LedgerCommand::Show { request_id } => self.ledger_show(&request_id),
            LedgerCommand::Allow { request_id } => self.ledger_decide(&request_id, true, caller),
            LedgerCommand::Deny { request_id } => self.ledger_decide(&request_id, false, caller),
        }
    }

    fn ledger_list(&self) -> KjResult {
        let rows = {
            let db = self.kernel_db.lock();
            match approval_ledger::ask::list_pending(db.conn_for_ledger()) {
                Ok(rows) => rows,
                Err(e) => return KjResult::Err(format!("kj ledger list: {e}")),
            }
        };
        let data = serde_json::Value::Array(
            rows.iter()
                .map(|r| serde_json::json!(r.request_id.clone()))
                .collect(),
        );
        if rows.is_empty() {
            return KjResult::ok_with_data("(no pending approvals)".to_string(), data);
        }
        let mut lines = vec![format!(
            "  {:<38}  {:<8}  {:<9}  {}",
            "REQUEST", "ORIGIN", "STATUS", "DESCRIPTION"
        )];
        for r in &rows {
            lines.push(format!(
                "  {:<38}  {:<8}  {:<9}  {}",
                r.request_id,
                r.origin,
                r.status,
                r.description,
            ));
        }
        lines.push(String::new());
        lines.push("answer with: kj ledger allow <request-id>  |  kj ledger deny <request-id>".into());
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    fn ledger_show(&self, request_id: &str) -> KjResult {
        let db = self.kernel_db.lock();
        let conn = db.conn_for_ledger();
        let row = match approval_ledger::ask::get_approval(conn, request_id) {
            Ok(Some(r)) => r,
            Ok(None) => {
                return KjResult::Err(format!("kj ledger: no such ask {request_id}"));
            }
            Err(e) => return KjResult::Err(format!("kj ledger show: {e}")),
        };
        let statements = match approval_ledger::ask::load_ask_statements(conn, request_id) {
            Ok(s) => s,
            Err(e) => return KjResult::Err(format!("kj ledger show: {e}")),
        };

        let mut lines = vec![
            format!("request:    {}", row.request_id),
            format!("status:     {}", row.status),
            format!("origin:     {}", row.origin),
            format!(
                "tool:       {}.{}",
                row.instance.as_deref().unwrap_or("-"),
                row.tool.as_deref().unwrap_or("-")
            ),
            format!("label:      {}", row.authorized_label.as_deref().unwrap_or("-")),
            format!("description: {}", row.description),
        ];
        for s in &statements {
            lines.push(format!("statement:  {}", s.statement.rendered));
        }
        if let Some(decided) = &row.decided_option {
            lines.push(format!("decided:    {decided}"));
        }
        // `context_id`, `instance`, `tool` and `hook_id` are here for a
        // client that has to ROUTE this ask, not just render it: an ACP
        // session, or the app, needs to know which context an ask belongs
        // to before it can decide whose screen to put it on. They were
        // missing while the only reader was a human at a shell, who
        // already knew.
        let data = serde_json::json!({
            "request_id": row.request_id,
            "context_id": ContextId::try_from_slice(&row.context_id).map(|c| c.to_string()),
            "status": row.status.to_string(),
            "origin": row.origin.to_string(),
            "instance": row.instance,
            "tool": row.tool,
            "hook_id": row.hook_id,
            "description": row.description,
            "authorized_label": row.authorized_label,
            "statements": statements.iter().map(|s| s.statement.rendered.clone()).collect::<Vec<_>>(),
        });
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    /// Claim + decide one ask as the calling principal.
    fn ledger_decide(&self, request_id: &str, allow: bool, caller: &KjCaller) -> KjResult {
        let verb = if allow { "allow" } else { "deny" };

        // The ledger work is scoped so the `KernelDb` guard is RELEASED before
        // the notification below. `announce_ledger_change` takes the same
        // mutex to read the committed generation, and `parking_lot::Mutex` is
        // not reentrant — announcing while still holding it would deadlock the
        // answerer, which is a far worse failure than the silent hang this
        // whole slice set out to fix.
        let result = {
            let db = self.kernel_db.lock();
            let conn = db.conn_for_ledger();
            let principal = caller.principal_id.as_bytes();

            match approval_ledger::claim::claim(conn, request_id, principal) {
                Ok(_) => {}
                // Nothing committed on any of these arms, so there is nothing
                // to announce — return straight out rather than falling
                // through to the notification below.
                Err(LedgerError::NotFound(_)) => {
                    return KjResult::Err(format!("kj ledger: no such ask {request_id}"));
                }
                Err(e @ LedgerError::NotClaimable { .. }) => {
                    // Someone else is answering, or the gate already expired it —
                    // say which, loudly; a silent no-op here reads as "done".
                    return KjResult::Err(format!("kj ledger: {e}"));
                }
                Err(e) => return KjResult::Err(format!("kj ledger: {e}")),
            }

            // Past the claim, a mutation HAS committed (pending → claimed), so
            // every path from here announces — including the error arms. A
            // failed decide after a successful claim still moved the ledger,
            // and a client rendering this ask as pending needs to know.
            match approval_ledger::decide::decide(
                conn,
                request_id,
                approval_ledger::decide::DecideInput {
                    allow,
                    decided_by: Some(principal),
                    decided_option: Some(if allow { "allow_once" } else { "deny" }),
                    remember_scope: None,
                    auto_reason: None,
                },
            ) {
                Ok(row) => KjResult::ok_with_data(
                    format!(
                        "{verb}ed ask {request_id} ({})",
                        row.description
                    ),
                    serde_json::json!({
                        "request_id": row.request_id,
                        "status": row.status.to_string(),
                        "verb": verb,
                    }),
                ),
                Err(LedgerError::AlreadyDecided { status, .. }) => KjResult::Err(format!(
                    "kj ledger: ask {request_id} was already decided ({status}) — \
                     your late answer was recorded in the event log but changed nothing"
                )),
                Err(e) => KjResult::Err(format!("kj ledger: {e}")),
            }
        };

        crate::kj::gate::announce_ledger_change(&self.kernel_db, self.kernel.ledger_flows());
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::gate::{run_gate, GateSpec};
    use crate::kj::test_helpers::{test_caller, test_dispatcher};
    use approval_ledger::types::VarBinding;
    use std::time::Duration;

    fn s(v: &str) -> String {
        v.to_string()
    }

    fn spec() -> GateSpec {
        GateSpec {
            origin: approval_ledger::types::Origin::KjVerb,
            instance: "builtin.kj".into(),
            tool: "cc.send".into(),
            hook_id: None,
            description: "test ask".into(),
            authorized_label: "some-target".into(),
            statements: vec![crate::kj::gate::GatedStatement {
                rendered: "kj cc send ${TARGET} ${MESSAGE}".into(),
                statement_kind: "kj_verb".into(),
                vars: vec![
                    ("TARGET".into(), VarBinding::Bound),
                    ("MESSAGE".into(), VarBinding::Free),
                ],
                source_index: None,
            }],
        }
    }

    #[tokio::test]
    async fn ledger_list_is_empty_then_shows_a_gated_ask() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let result = d.dispatch(&[s("ledger"), s("list")], &c).await;
        assert!(result.is_ok());
        assert!(result.message().contains("no pending approvals"));

        // Fire a gate with a long budget in the background, leaving a
        // pending ask; it must appear in the list.
        let db = d.kernel_db.clone();
        let caller = c.clone();
        let flows = d.kernel.ledger_flows().clone();
        let gate = tokio::spawn(async move {
            run_gate(&db, &caller, spec(), Duration::from_secs(30), &flows).await
        });
        let mut listed = String::new();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let result = d.dispatch(&[s("ledger"), s("list")], &c).await;
            if result.message().contains("test ask") {
                listed = result.message().to_string();
                break;
            }
        }
        assert!(listed.contains("test ask"), "list must show the ask: {listed}");
        assert!(listed.contains("kj ledger allow"));

        // Deny it through the verb; the gate must observe the refusal.
        let request_id = listed
            .lines()
            .find(|l| l.contains("test ask"))
            .and_then(|l| l.split_whitespace().next())
            .expect("request id is the first column")
            .to_string();
        let result = d
            .dispatch(&[s("ledger"), s("deny"), s(&request_id)], &c)
            .await;
        assert!(result.is_ok(), "deny must succeed: {result:?}");
        let outcome = gate.await.unwrap();
        assert!(!outcome.allowed());
        // A human said no. That IS a verdict — distinct from the gate
        // being unable to reach one.
        assert_eq!(outcome.verdict, crate::kj::gate::GateVerdict::Denied);
        assert_eq!(outcome.ask.expect("a denied ask has a row").request_id, request_id);
    }

    #[tokio::test]
    async fn ledger_allow_opens_the_gate_and_a_second_answer_is_loud() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let db = d.kernel_db.clone();
        let caller = c.clone();
        let flows = d.kernel.ledger_flows().clone();
        let gate = tokio::spawn(async move {
            run_gate(&db, &caller, spec(), Duration::from_secs(30), &flows).await
        });

        let mut request_id = String::new();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let db = d.kernel_db.lock();
            if let Some(row) = approval_ledger::ask::list_pending(db.conn_for_ledger())
                .unwrap()
                .first()
            {
                request_id = row.request_id.clone();
                break;
            }
        }
        assert!(!request_id.is_empty());

        let result = d
            .dispatch(&[s("ledger"), s("allow"), s(&request_id)], &c)
            .await;
        assert!(result.is_ok(), "allow must succeed: {result:?}");
        assert!(result.message().starts_with("allowed ask"));

        // Answering again is a loud error naming the terminal status, never
        // a silent no-op (guarantee 6).
        let again = d
            .dispatch(&[s("ledger"), s("allow"), s(&request_id)], &c)
            .await;
        assert!(!again.is_ok());
        assert!(again.message().contains("not `pending`") || again.message().contains("already"));

        let outcome = gate.await.unwrap();
        assert!(outcome.allowed());
    }

    #[tokio::test]
    async fn ledger_show_renders_the_statement_being_authorized() {
        let d = test_dispatcher().await;
        let c = test_caller();

        let db = d.kernel_db.clone();
        let caller = c.clone();
        let flows = d.kernel.ledger_flows().clone();
        let gate = tokio::spawn(async move {
            run_gate(&db, &caller, spec(), Duration::from_secs(30), &flows).await
        });

        let mut request_id = String::new();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let db = d.kernel_db.lock();
            if let Some(row) = approval_ledger::ask::list_pending(db.conn_for_ledger())
                .unwrap()
                .first()
            {
                request_id = row.request_id.clone();
                break;
            }
        }

        let result = d
            .dispatch(&[s("ledger"), s("show"), s(&request_id)], &c)
            .await;
        assert!(result.is_ok(), "{result:?}");
        let msg = result.message();
        assert!(msg.contains("kj cc send ${TARGET} ${MESSAGE}"), "{msg}");
        assert!(msg.contains("some-target"), "raw typed label, finding #3: {msg}");

        // The structured payload has to carry enough to ROUTE the ask, not
        // just print it: a client with several live sessions decides whose
        // screen this belongs on by its context, and cannot do that from
        // prose. Asserted here because the human-readable rendering above
        // would keep passing while a client silently lost the ability.
        let data = match &result {
            KjResult::Ok { data: Some(d), .. } => d.clone(),
            other => panic!("kj ledger show must emit structured data: {other:?}"),
        };
        assert_eq!(
            data["context_id"].as_str(),
            Some(c.context_id.expect("the test caller has a context").to_string().as_str()),
            "show must name the context the ask belongs to: {data}"
        );
        assert_eq!(data["instance"].as_str(), Some("builtin.kj"), "{data}");
        assert_eq!(data["tool"].as_str(), Some("cc.send"), "{data}");
        assert_eq!(data["request_id"].as_str(), Some(request_id.as_str()), "{data}");

        // Clean up the pending ask so the spawned gate terminates.
        d.dispatch(&[s("ledger"), s("deny"), s(&request_id)], &c)
            .await;
        let _ = gate.await;
    }

    #[tokio::test]
    async fn ledger_of_an_unknown_id_errors_loudly() {
        let d = test_dispatcher().await;
        let c = test_caller();
        let result = d
            .dispatch(&[s("ledger"), s("allow"), s("deadbeef")], &c)
            .await;
        assert!(!result.is_ok());
        assert!(result.message().contains("no such ask"));
    }

    mod dispatch_wiring {
        use super::*;

        #[tokio::test]
        async fn ledger_bare_renders_help() {
            let d = test_dispatcher().await;
            let c = test_caller();
            let result = d.dispatch(&[s("ledger")], &c).await;
            assert!(
                matches!(&result, KjResult::Ok { ephemeral: true, .. }),
                "kj ledger (no subcommand) should render help, got {result:?}"
            );
        }

        /// `kj ledger` reads the kernel DB, not a context — it must work
        /// from a shell with no context joined (the place a human goes to
        /// answer a gate).
        #[tokio::test]
        async fn ledger_works_without_a_joined_context() {
            let d = test_dispatcher().await;
            let c = KjCaller {
                principal_id: kaijutsu_types::PrincipalId::new(),
                context_id: None,
                session_id: kaijutsu_types::SessionId::new(),
                confirmed: false,
                rc_depth: 0,
                privileged: false,
            };
            let result = d.dispatch(&[s("ledger"), s("list")], &c).await;
            assert!(result.is_ok(), "{result:?}");
        }
    }
}
