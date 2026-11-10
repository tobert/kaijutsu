//! Records for tests across the crate: an open ask, its full record, and a
//! card over it, and the apps and ledger rows the asks' tests build on them.

use kaijutsu_client::LedgerState;
use kaijutsu_types::{AskDetail, AskOrigin, AskStatus, AskSummary, ContextId, PrincipalId, PrincipalRef};

use crate::app::App;

use super::{AnsweredRow, AskCardState, LedgerRow, Offer, PendingRow, RedeemedMark, fold_ledger, open_card};

pub fn summary(request_id: &str, ctx: ContextId, statement: &str) -> AskSummary {
    AskSummary {
        request_id: request_id.to_string(),
        status: AskStatus::Pending,
        origin: AskOrigin::ShellGate,
        context_id: Some(ctx),
        description: statement.to_string(),
        statements: vec![statement.to_string()],
        requester: None,
        performer: None,
        reviewer: None,
        created_at_ms: 0,
        decided_at_ms: None,
        tool_call_block_id: None,
        lineage_root: None,
        has_exec_source: false,
    }
}

pub fn detail(request_id: &str, ctx: ContextId, statement: &str) -> AskDetail {
    AskDetail {
        summary: summary(request_id, ctx, statement),
        instance: None,
        tool: Some("shell_write".to_string()),
        hook_id: None,
        label: None,
        exec_source: None,
        cwd: None,
        env: Vec::new(),
        decision: None,
        redeemed_at_ms: None,
        publication_abandoned: None,
        reassignments: Vec::new(),
    }
}

pub fn card(request_id: &str, ctx: ContextId, statement: &str) -> AskCardState {
    AskCardState { request_id: request_id.to_string(), context_id: ctx, detail: detail(request_id, ctx, statement), reassigning: false }
}

pub const REVIEWER: Offer = Offer { review: true, always: true, remember: false, escalate: false, take_over: false };

pub fn state(asks: &[AskSummary]) -> LedgerState {
    LedgerState::from_listing(1, asks.to_vec())
}

pub fn reviewer(id: PrincipalId, name: &str) -> Option<PrincipalRef> {
    Some(PrincipalRef { id, name: name.to_string() })
}

pub fn reviewed_by(me: PrincipalId, id: &str, ctx: ContextId) -> AskSummary {
    let mut ask = summary(id, ctx, "rm");
    ask.performer = reviewer(PrincipalId::new(), "coder");
    ask.reviewer = reviewer(me, "amy");
    ask
}

/// An app with `ids` open in the context on screen, reviewed by its own
/// principal, and the first one's card up.
pub fn app_with_card(ids: &[&str]) -> (App, ContextId) {
    let ctx = ContextId::new();
    let me = PrincipalId::new();
    let mut app = App::new("amy");
    app.current = Some(ctx);
    app.principal = Some(me);
    let open: Vec<AskSummary> = ids.iter().map(|id| reviewed_by(me, id, ctx)).collect();
    fold_ledger(&mut app, &state(&open));
    assert!(open_card(&mut app, detail(ids[0], ctx, "rm")));
    (app, ctx)
}

/// An ask the viewer reviews, with the shell program a rule needs.
pub fn app_with_program_card(configure: impl Fn(&mut AskSummary, PrincipalId)) -> (App, PrincipalId) {
    let ctx = ContextId::new();
    let me = PrincipalId::new();
    let mut app = App::new("amy");
    app.current = Some(ctx);
    app.principal = Some(me);
    let mut ask = reviewed_by(me, "a-1", ctx);
    ask.has_exec_source = true;
    configure(&mut ask, me);
    fold_ledger(&mut app, &state(&[ask]));
    assert!(open_card(&mut app, detail("a-1", ctx, "rm")));
    (app, me)
}

pub fn row_with(id: &str, offer: Offer) -> LedgerRow {
    LedgerRow::Pending(PendingRow {
        request_id: id.to_string(),
        age: None,
        context_label: "kaijutsu".into(),
        performer: "coder".into(),
        hook: "shell_write".into(),
        asker: Some("coder".into()),
        reviewer: Some("amy".into()),
        offer,
        statement: format!("echo {id}"),
    })
}

pub fn pending(id: &str, ctx: &str) -> LedgerRow {
    LedgerRow::Pending(PendingRow {
        request_id: id.to_string(),
        age: Some("12s".to_string()),
        context_label: ctx.to_string(),
        performer: "coder".to_string(),
        hook: "shell_write".to_string(),
        asker: Some("coder".to_string()),
        reviewer: Some("amy".to_string()),
        offer: REVIEWER,
        statement: "git worktree remove --force ~/src/wt/kaish-arith".to_string(),
    })
}

pub fn answered(id: &str, ctx: &str, redeemed: RedeemedMark) -> LedgerRow {
    LedgerRow::Answered(AnsweredRow {
        request_id: id.to_string(),
        time: Some("13:58".to_string()),
        context_label: ctx.to_string(),
        decision: Some("allow once".to_string()),
        principal: Some("amy".to_string()),
        redeemed,
        statement: "git worktree remove …".to_string(),
    })
}
