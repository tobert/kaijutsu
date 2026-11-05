//! Tests for `kj character retire` and the console rules: who may retire a
//! person or the performer of a person's console, and a retirement that
//! commits whole or not at all.

use std::sync::Arc;

use kaijutsu_types::{ContextId, PrincipalId};

use crate::kernel_db::CharacterRow;
use crate::kj::test_helpers::*;
use crate::kj::{KjCaller, KjDispatcher, KjResult};

fn s(v: &str) -> String {
    v.to_string()
}

/// Two persons, amy and bob, each with a root console. banto performs amy's
/// console; amy performs a Ctrl+Z context under her root console; amy's MCP
/// hub has one session, performed by an MCP connection's character; coder
/// performs a lane under amy's root console.
struct House {
    d: Arc<KjDispatcher>,
    amy: PrincipalId,
    bob: PrincipalId,
    banto: PrincipalId,
    coder: PrincipalId,
    mcp: PrincipalId,
    amy_root: ContextId,
    console: ContextId,
    shell: ContextId,
    hub: ContextId,
    session: ContextId,
    lane: ContextId,
}

async fn house() -> House {
    let d = Arc::new(test_dispatcher().await);
    d.set_self_arc();
    let [amy, bob, banto, coder, mcp] = std::array::from_fn(|_| PrincipalId::new());
    let amy_root = register_context(&d, Some("ret-amy"), None, amy);
    let bob_root = register_context(&d, Some("ret-bob"), None, bob);
    let console = register_context(&d, Some("console"), Some(amy_root), amy);
    let shell = register_context(&d, Some("ret-amy-shell"), Some(amy_root), amy);
    let hub = register_context(&d, Some("mcp/ret-amy"), Some(amy_root), amy);
    let session = register_context(&d, Some("session"), Some(hub), amy);
    let lane = register_context(&d, Some("lane"), Some(amy_root), amy);
    {
        let db = d.kernel_db().lock();
        for (principal_id, name, root, rc_dir) in [
            (amy, "ret-amy", true, "person"), (bob, "ret-bob", true, "person"), (banto, "ret-banto", false, "banto"),
            (coder, "ret-coder", false, "coder"), (mcp, "ret-mcp", false, "mcp"),
        ] {
            db.insert_character(&CharacterRow { rc_dir: Some(rc_dir.into()), console_ctx: None, mcp_hub_ctx: None,
                principal_id, name: s(name), created_at: 1, retired_at: None, handoff_ctx: None,
                root_ctx: None, root,
            }).unwrap();
        }
        for (context, performer, senpai) in [
            (amy_root, amy, None), (bob_root, bob, None), (console, banto, Some(amy_root)),
            (shell, amy, Some(amy_root)), (session, mcp, Some(amy_root)), (lane, coder, Some(amy_root)),
        ] {
            db.write_context_character(context, performer, senpai, None).unwrap();
        }
        db.set_context_senpai(hub, Some(amy_root)).unwrap();
        db.set_character_root_ctx(amy, Some(amy_root)).unwrap();
        db.set_character_root_ctx(bob, Some(bob_root)).unwrap();
        db.set_character_console_ctx(amy, Some(console)).unwrap();
        db.set_character_mcp_hub_ctx(amy, Some(hub)).unwrap();
    }
    House { d, amy, bob, banto, coder, mcp, amy_root, console, shell, hub, session, lane }
}

/// A confirmed caller acting as `actor` from `context`. `register_context`
/// binds every test context with `config-write`, as an MCP session's rc
/// binds it, so the capability check passes and the retire rules decide.
fn caller(context: ContextId, actor: PrincipalId) -> KjCaller {
    KjCaller { actor_id: actor, confirmed: true, ..caller_with_context(context) }
}

fn retire(name: &str) -> Vec<String> {
    vec![s("character"), s("retire"), s(name)]
}

fn retired(h: &House, who: PrincipalId) -> bool {
    h.d.kernel_db().lock().get_character(who).unwrap().unwrap().retired_at.is_some()
}

fn live(h: &House, context: ContextId) -> bool {
    let row = h.d.kernel_db().lock().get_context(context).unwrap().unwrap();
    !row.is_archived() && row.concluded_at.is_none()
}

/// An MCP session may not retire banto while banto performs amy's console:
/// only amy retires it. The refusal names the console and both fixes, and
/// nothing changes. Falsified by dropping the console check.
#[tokio::test]
async fn an_mcp_session_does_not_retire_the_performer_of_a_persons_console() {
    let h = house().await;
    let result = h.d.dispatch(&retire("ret-banto"), &caller(h.session, h.mcp)).await;
    let KjResult::Err(message) = result else { panic!("expected a refusal, got {result:?}") };
    assert!(message.contains("console") && message.contains("ret-amy"), "{message}");
    assert!(message.contains("kj character set ret-amy --console <ctx>") && message.contains("--no-console"), "{message}");
    assert!(message.contains("archive"), "{message}");
    assert!(!retired(&h, h.banto));
    assert!(live(&h, h.console));
    assert_eq!(h.d.kernel_db().lock().get_character(h.amy).unwrap().unwrap().console_ctx, Some(h.console));
}

/// Amy retires banto: the retirement archives her console and clears her
/// `console_ctx` with it.
#[tokio::test]
async fn the_person_retires_the_performer_of_her_console() {
    let h = house().await;
    let result = h.d.dispatch(&retire("ret-banto"), &caller(h.amy_root, h.amy)).await;
    assert!(result.is_ok(), "{}", result.message());
    assert!(retired(&h, h.banto));
    assert!(h.d.kernel_db().lock().get_context(h.console).unwrap().unwrap().is_archived());
    assert_eq!(h.d.kernel_db().lock().get_character(h.amy).unwrap().unwrap().console_ctx, None);
}

/// An MCP session may not retire a person. Falsified by letting any caller
/// with `config-write` retire a root character.
#[tokio::test]
async fn an_mcp_session_does_not_retire_a_person() {
    let h = house().await;
    let result = h.d.dispatch(&retire("ret-amy"), &caller(h.session, h.mcp)).await;
    let KjResult::Err(message) = result else { panic!("expected a refusal, got {result:?}") };
    assert!(message.contains("ret-amy") && message.contains("person"), "{message}");
    assert!(!retired(&h, h.amy));
    assert!(live(&h, h.amy_root) && live(&h, h.shell));
}

/// Bob, a live person, retires amy: her root console and her Ctrl+Z context
/// are archived with her pointers cleared, and what others perform (her
/// console, the session, the lane) and the hub stay live. Falsified by the
/// archive write refusing her root console, or by running the console rule
/// on a root console the person performs.
#[tokio::test]
async fn a_person_retires_another_person_and_archives_her_root_console() {
    let h = house().await;
    let result = h.d.dispatch(&retire("ret-amy"), &caller(h.bob_root(), h.bob)).await;
    assert!(result.is_ok(), "{}", result.message());
    assert!(retired(&h, h.amy));
    let db = h.d.kernel_db().lock();
    for context in [h.amy_root, h.shell] {
        assert!(db.get_context(context).unwrap().unwrap().is_archived(), "{}", db.context_name(context));
    }
    for context in [h.console, h.session, h.lane, h.hub] {
        assert!(!db.get_context(context).unwrap().unwrap().is_archived(), "{}", db.context_name(context));
    }
    let sheet = db.get_character(h.amy).unwrap().unwrap();
    assert_eq!((sheet.root_ctx, sheet.console_ctx, sheet.mcp_hub_ctx), (None, None, None));
}

/// A retirement commits whole or not at all. When one of amy's contexts
/// cannot be archived, she is not retired, keeps every pointer, and every
/// context she performs stays live and unconcluded. Falsified by
/// concluding or archiving outside the transaction that retires.
#[tokio::test]
async fn a_retirement_that_cannot_archive_one_context_changes_nothing() {
    let h = house().await;
    let stuck = register_context(&h.d, Some("stuck"), Some(h.amy_root), h.amy);
    {
        let mut db = h.d.kernel_db().lock();
        db.write_context_character(stuck, h.amy, Some(h.amy_root), None).unwrap();
        db.conn_for_test_mut().execute_batch(
            "CREATE TEMP TRIGGER stuck_stays_live BEFORE UPDATE OF archived_at ON contexts
             WHEN OLD.label = 'stuck' BEGIN SELECT RAISE(ABORT, 'injected archive fault'); END",
        ).unwrap();
    }
    let result = h.d.dispatch(&retire("ret-amy"), &caller(h.bob_root(), h.bob)).await;
    let KjResult::Err(message) = result else { panic!("expected the fault, got {result:?}") };
    assert!(message.contains("injected archive fault"), "{message}");
    assert!(!retired(&h, h.amy));
    for context in [h.amy_root, h.shell, stuck] {
        assert!(live(&h, context), "{} stays live and unconcluded", h.d.kernel_db().lock().context_name(context));
    }
    let sheet = h.d.kernel_db().lock().get_character(h.amy).unwrap().unwrap();
    assert_eq!(
        (sheet.root_ctx, sheet.console_ctx, sheet.mcp_hub_ctx),
        (Some(h.amy_root), Some(h.console), Some(h.hub)),
        "every pointer stays"
    );
}

/// A played context that holds a drift refuses the retirement before
/// anything is written: banto is not retired, and its console stays live,
/// unconcluded, and amy's. Falsified by concluding before the drift check.
#[tokio::test]
async fn a_held_drift_refuses_the_retirement_before_anything_changes() {
    let h = house().await;
    for context in [h.console, h.lane] {
        h.d.block_store().create_document(context, crate::DocumentKind::Conversation, None).unwrap();
    }
    let pushed = h.d.dispatch(&[s("drift"), s("push"), s("console"), s("hold")], &caller(h.lane, h.coder)).await;
    assert!(pushed.is_ok(), "{}", pushed.message());
    assert!(h.d.kernel_db().lock().held_drift_refusal(h.console).unwrap().is_some(), "the console holds the drift");

    let result = h.d.dispatch(&retire("ret-banto"), &caller(h.amy_root, h.amy)).await;
    let KjResult::Err(message) = result else { panic!("expected a refusal, got {result:?}") };
    assert!(message.contains("drift") && message.contains("kj ledger"), "{message}");
    assert!(!retired(&h, h.banto));
    assert!(live(&h, h.console));
    assert_eq!(h.d.kernel_db().lock().get_character(h.amy).unwrap().unwrap().console_ctx, Some(h.console));
}

impl House {
    fn bob_root(&self) -> ContextId {
        self.d.kernel_db().lock().get_character(self.bob).unwrap().unwrap().root_ctx.expect("bob has a root console")
    }
}
