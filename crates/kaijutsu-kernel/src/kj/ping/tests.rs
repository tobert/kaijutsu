use std::time::Duration;

use kaijutsu_types::{BlockKind, ContentType, ContextId, ContextState, PrincipalId, Role, SessionId, Status};

use super::super::test_helpers::{caller_with_context, register_context, test_dispatcher};
use crate::flows::TurnFlow;
use crate::kernel_db::{CharacterRow, ContextRow};
use crate::kj::KjCaller;

fn s(x: &str) -> String {
    x.to_string()
}

fn seed_with_block(d: &crate::kj::KjDispatcher, ctx: ContextId, author: PrincipalId) {
    if !d.block_store().contains(ctx) {
        d.block_store()
            .create_document(ctx, kaijutsu_types::DocKind::Conversation, None)
            .unwrap();
    }
    d.block_store()
        .insert_block_as(
            ctx,
            None,
            None,
            Role::User,
            BlockKind::Text,
            "seed".to_string(),
            Status::Done,
            ContentType::Plain,
            Some(author),
        )
        .unwrap();
}

fn caller_as(principal_id: PrincipalId, context_id: Option<ContextId>) -> KjCaller {
    KjCaller {
        principal_id,
        actor_id: principal_id,
        reviewer_id: None,
        context_id,
        session_id: SessionId::new(),
        confirmed: false,
        rc_depth: 0,
        privileged: false,
        cancel: tokio_util::sync::CancellationToken::new(),
    }
}

fn insert_char(
    d: &crate::kj::KjDispatcher,
    principal_id: PrincipalId,
    name: &str,
    root: bool,
) {
    d.kernel_db()
        .lock()
        .insert_character(&CharacterRow {
            principal_id,
            name: name.into(),
            created_at: 1,
            retired_at: None,
            handoff_ctx: None,
            root_ctx: None,
            root,
            rc_dir: Some(if root { "person" } else { "coder" }.into()),
            console_ctx: None,
            mcp_hub_ctx: None,
        })
        .unwrap();
}

fn register_live_context(
    d: &crate::kj::KjDispatcher,
    label: &str,
    senpai: Option<ContextId>,
    performer: PrincipalId,
) -> ContextId {
    let ctx = register_context(d, Some(label), senpai, performer);
    d.kernel_db()
        .lock()
        .update_performer(ctx, Some(performer))
        .unwrap();
    seed_with_block(d, ctx, performer);
    ctx
}

#[tokio::test]
async fn worker_without_drive_can_ping_sibling_and_writes_trusted_notice() {
    let d = test_dispatcher().await;
    let banto_id = PrincipalId::new();
    let alice_id = PrincipalId::new();
    let bob_id = PrincipalId::new();
    insert_char(&d, banto_id, "ping-banto", false);
    insert_char(&d, alice_id, "ping-alice", false);
    insert_char(&d, bob_id, "ping-bob", false);

    let senpai = register_live_context(&d, "banto-room", None, banto_id);
    let alice = register_live_context(&d, "alice-lane", Some(senpai), alice_id);
    let bob = register_live_context(&d, "bob-lane", Some(senpai), bob_id);

    // Narrow alice's loadout to worker-only (no drive, senpai, operator, house).
    d.kernel_db()
        .lock()
        .upsert_context_binding(alice, &crate::mcp::ContextToolBinding::new())
        .unwrap();

    let mut sub = d.kernel().turn_flows().subscribe("turn.requested");
    let caller = caller_with_context(alice).with_actor(alice_id, Some(banto_id));

    let res = d.dispatch(&[s("ping"), s("bob-lane")], &caller).await;
    assert!(res.is_ok(), "ping failed: {}", res.message());
    assert_eq!(res.message(), "pinged 'bob-lane'");

    // Return data is opaque: carries only context_id, no busy/turn_id/accepted state.
    if let crate::kj::KjResult::Ok { data: Some(val), .. } = &res {
        assert_eq!(val["context_id"], bob.to_hex());
        assert!(val.get("busy").is_none());
        assert!(val.get("turn_id").is_none());
        assert!(val.get("accepted").is_none());
    } else {
        panic!("expected Ok with data");
    }

    // Target received a kernel-authored System/Notification block.
    let blocks = d.block_store().block_snapshots(bob).unwrap();
    let last = blocks.last().unwrap();
    assert_eq!(last.role, Role::System);
    assert_eq!(last.kind, BlockKind::Notification);
    assert_eq!(last.id.principal_id, PrincipalId::system());
    assert_eq!(last.content, "Ping from alice-lane (ping-alice, sibling).");

    // Target turn was requested.
    let ev = tokio::time::timeout(Duration::from_secs(2), sub.recv())
        .await
        .expect("timed out waiting for TurnFlow::Requested")
        .expect("turn flow channel closed");
    assert!(matches!(ev.payload, TurnFlow::Requested { context_id, .. } if context_id == bob));
}

#[tokio::test]
async fn ping_attaches_recent_shared_board_pointers_without_post_body() {
    let d = test_dispatcher().await;
    let banto_id = PrincipalId::new();
    let alice_id = PrincipalId::new();
    let bob_id = PrincipalId::new();
    insert_char(&d, banto_id, "hint-banto", false);
    insert_char(&d, alice_id, "hint-alice", false);
    insert_char(&d, bob_id, "hint-bob", false);

    let senpai = register_live_context(&d, "hint-banto-ctx", None, banto_id);
    let alice = register_live_context(&d, "hint-alice-ctx", Some(senpai), alice_id);
    let bob = register_live_context(&d, "hint-bob-ctx", Some(senpai), bob_id);

    // Alice posts a topic to `.senpai` (the shared coordination board).
    let alice_caller = caller_with_context(alice).with_actor(alice_id, Some(banto_id));
    let post = d
        .dispatch(
            &[
                s("board"),
                s("post"),
                s(".senpai"),
                s("--title"),
                s("CI Failures"),
                s("secret-body-text-must-not-appear-in-ping"),
            ],
            &alice_caller,
        )
        .await;
    assert!(post.is_ok(), "board post failed: {}", post.message());

    // Alice pings bob (sibling) and senpai.
    let res_bob = d.dispatch(&[s("ping"), s("hint-bob-ctx")], &alice_caller).await;
    assert!(res_bob.is_ok(), "ping bob failed: {}", res_bob.message());
    let bob_notice = d.block_store().block_snapshots(bob).unwrap().pop().unwrap();
    assert!(
        bob_notice.content.contains("Recent board posts by hint-alice:\n- \"CI Failures\" #1 — `kj board read '"),
        "unexpected bob notice: {}",
        bob_notice.content
    );
    assert!(
        !bob_notice.content.contains(".senpai") && !bob_notice.content.contains("./"),
        "ping notice must not use relative board paths: {}",
        bob_notice.content
    );
    assert!(
        !bob_notice.content.contains("secret-body-text"),
        "ping notice must never leak post bodies"
    );

    let res_senpai = d.dispatch(&[s("ping"), s(".senpai")], &alice_caller).await;
    assert!(res_senpai.is_ok(), "ping senpai failed: {}", res_senpai.message());
    let senpai_notice = d.block_store().block_snapshots(senpai).unwrap().pop().unwrap();
    assert!(
        senpai_notice.content.contains("your kohai"),
        "expected kohai relation: {}",
        senpai_notice.content
    );
    assert!(
        senpai_notice.content.contains("- \"CI Failures\" #1 — `kj board read '"),
        "expected board read hint: {}",
        senpai_notice.content
    );
}

#[tokio::test]
async fn ping_while_target_is_busy_offers_live_input_without_stacking_turns() {
    let d = test_dispatcher().await;
    let alice_id = PrincipalId::new();
    let bob_id = PrincipalId::new();
    insert_char(&d, alice_id, "busy-alice", false);
    insert_char(&d, bob_id, "busy-bob", false);

    let alice = register_live_context(&d, "busy-alice-ctx", None, alice_id);
    let bob = register_live_context(&d, "busy-bob-ctx", None, bob_id);

    // Simulate an active turn holding bob's ingress open.
    let lease = d.kernel().turns().begin(bob);
    d.kernel()
        .turns()
        .open_ingress(bob, lease.id(), lease.interrupt());

    let mut sub = d.kernel().turn_flows().subscribe("turn.requested");
    let caller = caller_with_context(alice).with_actor(alice_id, None);
    let res = d.dispatch(&[s("ping"), s("busy-bob-ctx")], &caller).await;
    assert!(res.is_ok(), "ping while busy failed: {}", res.message());

    // No second TurnFlow::Requested was published while bob is busy.
    assert!(
        tokio::time::timeout(Duration::from_millis(80), sub.recv())
            .await
            .is_err(),
        "ping must not stack a concurrent turn on a busy context"
    );

    // Closing ingress returns the undelivered Wake::Ping input.
    let undelivered = d.kernel().turns().close_ingress(bob, lease.id());
    assert_eq!(undelivered.len(), 1);
    assert!(matches!(
        undelivered[0].wake,
        crate::runtime::turn_state::Wake::Ping { principal } if principal == alice_id
    ));
    assert!(undelivered[0].wake.continues_turn());
    drop(lease);
}

#[tokio::test]
async fn ping_refuses_console_and_self_and_respects_character_no_pings() {
    let d = test_dispatcher().await;
    let amy_id = PrincipalId::new();
    let alice_id = PrincipalId::new();
    let bob_id = PrincipalId::new();
    insert_char(&d, amy_id, "ref-amy", true);
    insert_char(&d, alice_id, "ref-alice", false);
    insert_char(&d, bob_id, "ref-bob", false);

    let alice = register_live_context(&d, "ref-alice-ctx", None, alice_id);
    let console = register_live_context(&d, "amy-console", None, bob_id);
    let bob = register_live_context(&d, "ref-bob-ctx", None, bob_id);
    d.kernel_db()
        .lock()
        .set_character_console_ctx(amy_id, Some(console))
        .unwrap();

    let caller = caller_with_context(alice).with_actor(alice_id, None);

    // Self-ping is refused.
    let self_err = d.dispatch(&[s("ping"), s(".")], &caller).await;
    assert!(!self_err.is_ok());
    assert!(self_err.message().contains("own context"), "{}", self_err.message());

    // Console target is refused.
    let console_err = d.dispatch(&[s("ping"), s("amy-console")], &caller).await;
    assert!(!console_err.is_ok());
    assert!(console_err.message().contains("console"), "{}", console_err.message());

    // Character opt-out via `kj character set ref-bob --no-pings`.
    let mut root_caller = caller_as(amy_id, None);
    root_caller.privileged = true;
    let disable = d
        .dispatch(&[s("character"), s("set"), s("ref-bob"), s("--no-pings")], &root_caller)
        .await;
    assert!(disable.is_ok(), "character set --no-pings failed: {}", disable.message());
    let show = d
        .dispatch(&[s("character"), s("show"), s("ref-bob")], &root_caller)
        .await;
    assert!(show.is_ok());
    assert!(show.message().contains("Pings:           no"), "{}", show.message());

    let opt_out_err = d.dispatch(&[s("ping"), s("ref-bob-ctx")], &caller).await;
    assert!(!opt_out_err.is_ok());
    assert!(
        opt_out_err.message().contains("does not accept pings"),
        "{}",
        opt_out_err.message()
    );

    // Re-enabling via `--pings` lets pings through again.
    let enable = d
        .dispatch(&[s("character"), s("set"), s("ref-bob"), s("--pings")], &root_caller)
        .await;
    assert!(enable.is_ok(), "character set --pings failed: {}", enable.message());
    let ok = d.dispatch(&[s("ping"), s("ref-bob-ctx")], &caller).await;
    assert!(ok.is_ok(), "ping after --pings failed: {}", ok.message());
    assert!(d.block_store().block_snapshots(bob).unwrap().len() >= 2);
}

#[tokio::test]
async fn ping_to_non_turn_contexts_is_opaque_noop() {
    let d = test_dispatcher().await;
    let amy_id = PrincipalId::new();
    let alice_id = PrincipalId::new();
    let bob_id = PrincipalId::new();
    insert_char(&d, amy_id, "noop-amy", true);
    insert_char(&d, alice_id, "noop-alice", false);
    insert_char(&d, bob_id, "noop-bob", false);

    let alice = register_live_context(&d, "noop-alice-ctx", None, alice_id);
    let person_ctx = register_live_context(&d, "amy-shell", None, amy_id);
    let archived_ctx = register_live_context(&d, "old-lane", None, bob_id);
    d.kernel_db().lock().archive_context(archived_ctx).unwrap();

    // Verify archived context row has archived_at set.
    let row: ContextRow = d.kernel_db().lock().get_context(archived_ctx).unwrap().unwrap();
    assert!(row.is_archived() || row.context_state == ContextState::Archived);

    let caller = caller_with_context(alice).with_actor(alice_id, None);
    let mut sub = d.kernel().turn_flows().subscribe("turn.requested");

    let archived_hex = archived_ctx.to_hex();
    for (target, arg, expected_label) in [
        (person_ctx, "amy-shell", "amy-shell"),
        (archived_ctx, archived_hex.as_str(), "old-lane"),
    ] {
        let before = d.block_store().block_snapshots(target).unwrap().len();
        let res = d.dispatch(&[s("ping"), s(arg)], &caller).await;
        assert!(res.is_ok(), "expected opaque ack for {expected_label}: {}", res.message());
        assert_eq!(res.message(), format!("pinged '{expected_label}'"));
        assert_eq!(
            d.block_store().block_snapshots(target).unwrap().len(),
            before,
            "no block should be written to non-turn context {expected_label}"
        );
    }

    assert!(
        tokio::time::timeout(Duration::from_millis(80), sub.recv())
            .await
            .is_err(),
        "no turn should be requested for non-turn contexts"
    );
}

#[tokio::test]
async fn ping_enforces_per_sender_target_rate_limit() {
    let d = test_dispatcher().await;
    d.ping_rate_limiter().configure(2, 60_000);

    let alice_id = PrincipalId::new();
    let bob_id = PrincipalId::new();
    insert_char(&d, alice_id, "rl-alice", false);
    insert_char(&d, bob_id, "rl-bob", false);

    let alice = register_live_context(&d, "rl-alice-ctx", None, alice_id);
    let _bob = register_live_context(&d, "rl-bob-ctx", None, bob_id);
    let caller = caller_with_context(alice).with_actor(alice_id, None);

    assert!(d.dispatch(&[s("ping"), s("rl-bob-ctx")], &caller).await.is_ok());
    assert!(d.dispatch(&[s("ping"), s("rl-bob-ctx")], &caller).await.is_ok());
    let third = d.dispatch(&[s("ping"), s("rl-bob-ctx")], &caller).await;
    assert!(!third.is_ok());
    assert!(
        third.message().contains("rate limit exceeded"),
        "{}",
        third.message()
    );
}

#[tokio::test]
async fn ping_help_text_is_clean() {
    let d = test_dispatcher().await;
    let alice_id = PrincipalId::new();
    insert_char(&d, alice_id, "help-alice", false);
    let alice = register_live_context(&d, "help-alice-ctx", None, alice_id);
    let caller = caller_with_context(alice).with_actor(alice_id, None);

    let help = d.dispatch(&[s("ping"), s("--help")], &caller).await;
    assert!(help.is_ok());
    assert!(help.message().contains("Usage: ping <TARGET>"), "{}", help.message());
    assert!(help.message().contains(".senpai"), "{}", help.message());
}
