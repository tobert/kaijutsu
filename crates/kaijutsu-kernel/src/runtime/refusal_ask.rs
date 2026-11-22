//! The ask a refused turn leaves for its reviewer.
//!
//! A provider's refusal stops the turn and holds the context
//! ([`Kernel::refusal_hold`]). The ask makes that stop a decision a person
//! sees: an allow lifts the hold and sends the context's work again, and a
//! deny leaves it held. The ask holds no source and runs nothing.

use std::sync::Arc;
use kaijutsu_types::{ContextId, PrincipalId};
use crate::Kernel;

impl Kernel {
    /// Open the ask for a context whose latest turn the provider refused,
    /// addressed to the reviewer the senpai chain gives that context. Returns
    /// the new ask's id, or `None` when an earlier refusal ask is still open
    /// or answered and not yet settled.
    pub(crate) fn open_refusal_ask(&self, context: ContextId) -> Result<Option<String>, String> {
        use approval_ledger::types::{NewAsk, NewOption, Origin};
        let request_id = {
            let db = self.kernel_db().lock();
            let open = approval_ledger::ask::list_unsettled_asks(db.conn_for_ledger(), context.as_bytes(), Origin::Refusal)
                .map_err(|e| format!("could not read the context's refusal asks: {e}"))?;
            if !open.is_empty() {
                return Ok(None);
            }
            let performer = db.context_character(context)
                .map_err(|e| format!("could not read the context's character: {e}"))?
                .ok_or_else(|| format!("context {} has no performer to ask for", context.short()))?
                .principal_id;
            let reviewer = db.route_review(crate::kernel_db::ReviewQuery::ask(context, performer))
                .map_err(|e| format!("could not resolve a reviewer for the refusal: {e}"))?
                .reviewer();
            let name = db.context_name(context);
            let ask = NewAsk {
                context_id: context.as_bytes().to_vec(),
                actor_id: performer.as_bytes().to_vec(),
                reviewer_id: reviewer.as_bytes().to_vec(),
                principal_id: performer.as_bytes().to_vec(),
                origin: Origin::Refusal,
                instance: Some("builtin.kj".into()),
                tool: Some("refusal".into()),
                hook_id: None,
                description: format!(
                    "The provider declined the last request of {name}, so the context is held. \
                     Approving sends its work again, and it may be declined again. \
                     `kj context rotate` starts fresh from a handoff."
                ),
                statements: vec![],
                authorized_label: None,
                rc_run_id: None,
                expires_at: None,
                options: vec![
                    NewOption { option_id: "allow_once".into(), label: "Send the work again".into(), kind: "allow_once".into() },
                    NewOption { option_id: "deny".into(), label: "Keep it held".into(), kind: "deny".into() },
                ],
                signals: vec![],
                cwd: None,
                exec_source: None,
                exec_stdin: None,
                continuation_epoch: None,
                env: vec![],
            };
            db.create_approval_ask_recorded(&ask, false, |_, _| Ok(()))
                .map_err(|e| format!("could not record the refusal ask: {e}"))?
        };
        crate::kj::gate::announce_ledger_change(self.kernel_db(), self.ledger_flows());
        Ok(Some(request_id))
    }

    /// Act on an answered refusal ask, once. The answer is spent and, for an
    /// allow, the hold is lifted in one transaction, so a repeat finds the
    /// answer spent. An allow then requests an autonomous turn, which opens a
    /// new continuation epoch; it lifts nothing when a newer turn already
    /// replaced the refusal. A deny changes nothing.
    pub(crate) fn settle_refusal_answer(self: &Arc<Self>, request_id: &str, allowed: bool) -> Result<(), String> {
        let row = self.kernel_db().lock().get_approval(request_id)
            .map_err(|e| format!("could not read the refusal ask: {e}"))?
            .ok_or_else(|| format!("refusal ask {request_id} has no row"))?;
        let context = ContextId::try_from_slice(&row.context_id)
            .ok_or_else(|| format!("refusal ask {request_id} has no context"))?;
        let resumed = self.kernel_db().lock().in_transaction(|db| {
            if !db.redeem_ask(request_id)? {
                return Ok(false);
            }
            Ok(allowed && db.release_refusal_hold(context)?)
        }).map_err(|e| format!("could not spend the refusal ask's answer: {e}"))?;
        crate::kj::gate::announce_ledger_change(self.kernel_db(), self.ledger_flows());
        if !resumed {
            return Ok(());
        }
        let principal = row.decided_by.as_deref().and_then(PrincipalId::try_from_slice)
            .or_else(|| row.actor_id.as_deref().and_then(PrincipalId::try_from_slice))
            .ok_or_else(|| format!("refusal ask {request_id} names nobody to resume as"))?;
        let after = self.blocks().last_block_id(context)
            .ok_or_else(|| format!("context {} has no blocks to anchor a turn after", context.short()))?;
        self.request_turn(super::turn_request::TurnRequest {
            context_id: context, after_block_id: after, content: String::new(),
            principal_id: principal, continuation_epoch: None, score: None,
        }).map(|_| ()).map_err(|e| format!("the hold is lifted but the turn was not admitted: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approval_ledger::types::{ApprovalRow, Origin};
    use kaijutsu_types::{AskVerdict, PrincipalId};
    use crate::kj::test_helpers::{register_context, test_dispatcher};

    struct House {
        d: crate::kj::KjDispatcher,
        amy: PrincipalId,
        banto: PrincipalId,
        coder: PrincipalId,
        banto_seat: ContextId,
        lane: ContextId,
    }

    /// `amy`, a person, holds the root; `banto`, a model, holds a seat under
    /// it; `coder`, a model, plays `lane` under that seat.
    async fn house() -> House {
        let d = test_dispatcher().await;
        let [amy, banto, coder] = std::array::from_fn(|_| PrincipalId::new());
        let root = register_context(&d, Some("amy"), None, amy);
        let banto_seat = register_context(&d, Some("banto-seat"), Some(root), amy);
        let lane = register_context(&d, Some("lane"), Some(banto_seat), amy);
        {
            let db = d.kernel_db().lock();
            for (principal_id, name, root) in [(amy, "refusal-amy", true), (banto, "refusal-banto", false), (coder, "refusal-coder", false)] {
                db.insert_character(&crate::kernel_db::CharacterRow { rc_dir: Some(if root { "person" } else { "coder" }.into()),
                    console_ctx: None, mcp_hub_ctx: None, principal_id, name: name.into(), created_at: 1, retired_at: None,
                    handoff_ctx: None, root_ctx: None, root,
                }).unwrap();
            }
            for model in [banto, coder] {
                db.seed_test_slot_entries(model, &[("model", "mock", "mock-model")]).unwrap();
            }
            for (context, performer, reviewer) in [(root, amy, None), (banto_seat, banto, Some(amy)), (lane, coder, Some(banto))] {
                db.assign_test_review(context, Some(performer), reviewer).unwrap();
            }
            db.set_character_root_ctx(amy, Some(root)).unwrap();
        }
        for context in [banto_seat, lane] {
            d.block_store().create_document(context, crate::DocumentKind::Conversation, None).unwrap();
            d.block_store().insert_block_as(context, None, None, kaijutsu_types::Role::User, kaijutsu_types::BlockKind::Text,
                "work", kaijutsu_types::Status::Done, kaijutsu_types::ContentType::Plain, Some(amy)).unwrap();
        }
        House { d, amy, banto, coder, banto_seat, lane }
    }

    /// The context's latest turn ends refused, as `settle_refusal`'s caller records it.
    fn refuse(kernel: &Kernel, context: ContextId, performer: PrincipalId, at: i64) {
        let turn_id = kaijutsu_types::TurnId::new();
        let db = kernel.kernel_db().lock();
        db.begin_turn(&crate::TurnStart { turn_id, context_id: context, performer, role: "model".into(),
            provider: "mock".into(), model: "mock-model".into(), started_at: at }).unwrap();
        db.finish_turn(turn_id, at + 1, "refused").unwrap();
    }

    fn refusal_asks(kernel: &Kernel, context: ContextId) -> Vec<ApprovalRow> {
        let db = kernel.kernel_db().lock();
        let mut rows = approval_ledger::ask::list_pending(db.conn_for_ledger()).unwrap();
        rows.retain(|row| row.origin == Origin::Refusal && row.context_id == context.as_bytes());
        rows
    }

    /// A refusal opens one ask for the context's reviewer. Its text names the
    /// context and the ways forward and carries none of the provider's words.
    #[tokio::test]
    async fn a_refusal_opens_one_ask_for_its_reviewer_and_stacks_no_second() {
        let h = house().await;
        let kernel = h.d.kernel();
        refuse(kernel, h.lane, h.coder, 1_000);
        let id = kernel.open_refusal_ask(h.lane).unwrap().expect("a refusal opens an ask");
        let asks = refusal_asks(kernel, h.lane);
        let [ask] = &asks[..] else { panic!("one ask: {asks:?}") };
        assert_eq!(ask.request_id, id);
        assert!(ask.exec_source.is_none() && ask.hook_id.is_none(), "it carries no source");
        let text = &ask.description;
        for needed in ["lane", "declined", "approving", "again", "kj context rotate"] {
            assert!(text.to_lowercase().contains(needed), "the ask says {needed:?}: {text}");
        }
        assert_eq!(ask.reviewer_id.as_deref(), Some(h.banto.as_bytes().as_slice()), "a kohai's reviewer is its senpai's performer");

        assert_eq!(kernel.open_refusal_ask(h.lane).unwrap(), None, "a second refusal does not stack another ask");
        assert_eq!(refusal_asks(kernel, h.lane).len(), 1);

        refuse(kernel, h.banto_seat, h.banto, 2_000);
        kernel.open_refusal_ask(h.banto_seat).unwrap().expect("the seat's refusal opens its own ask");
        let seat_asks = refusal_asks(kernel, h.banto_seat);
        assert_eq!(seat_asks[0].reviewer_id.as_deref(), Some(h.amy.as_bytes().as_slice()), "a senpai's reviewer is the person");
    }

    /// `kj ledger list` and `show` render the ask by its origin.
    #[tokio::test]
    async fn the_ledger_lists_and_shows_a_refusal_ask() {
        let h = house().await;
        refuse(h.d.kernel(), h.lane, h.coder, 1_000);
        let id = h.d.kernel().open_refusal_ask(h.lane).unwrap().unwrap();
        let caller = crate::kj::KjCaller { actor_id: h.banto, ..crate::kj::test_helpers::caller_with_context(h.banto_seat) };
        let s = |t: &str| t.to_string();
        let listed = h.d.dispatch(&[s("ledger"), s("list"), s("--origin"), s("refusal")], &caller).await;
        assert!(listed.is_ok(), "{}", listed.message());
        assert!(listed.message().contains(&id) && listed.message().contains("refusal"), "{}", listed.message());
        let shown = h.d.dispatch(&[s("ledger"), s("show"), id.clone()], &caller).await;
        assert!(shown.is_ok(), "{}", shown.message());
        assert!(shown.message().contains("refusal") && shown.message().contains("declined"), "{}", shown.message());
    }

    async fn requested(kernel: &Arc<Kernel>) -> crate::flows::Subscription<crate::flows::TurnFlow> {
        kernel.turn_flows().subscribe("turn.requested")
    }

    /// An allow lifts the hold for that one request and requests one turn,
    /// whatever the hold said before; settling it twice requests nothing more.
    #[tokio::test]
    async fn an_allow_resumes_the_refused_context_once() {
        let h = house().await;
        let kernel = h.d.kernel();
        refuse(kernel, h.lane, h.coder, 1_000);
        assert!(kernel.refusal_hold(h.lane).is_err(), "the context is held");
        let id = kernel.open_refusal_ask(h.lane).unwrap().unwrap();
        let mut turns = requested(kernel).await;

        crate::kj::ledger::decide_ask(kernel, h.banto, Some(h.banto_seat), &id, AskVerdict::Allow, None).expect("the reviewer allows");
        kernel.settle_refusal_answer(&id, true).unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), turns.recv()).await
            .expect("an allow requests a turn").unwrap();
        assert!(matches!(&event.payload, crate::flows::TurnFlow::Requested { context_id, .. } if *context_id == h.lane), "{:?}", event.payload);
        assert!(kernel.refusal_hold(h.lane).is_ok(), "the hold is lifted until a newer turn ends refused");

        kernel.settle_refusal_answer(&id, true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(turns.try_recv().is_none(), "the answer is spent: no second turn");
        assert!(kernel.kernel_db().lock().ask_redeemed(&id).unwrap());
        kernel.shutdown_runtime_worker().await.unwrap();
    }

    /// A deny leaves the context held and requests nothing.
    #[tokio::test]
    async fn a_deny_leaves_the_context_held() {
        let h = house().await;
        let kernel = h.d.kernel();
        refuse(kernel, h.lane, h.coder, 1_000);
        let id = kernel.open_refusal_ask(h.lane).unwrap().unwrap();
        let mut turns = requested(kernel).await;
        crate::kj::ledger::decide_ask(kernel, h.banto, Some(h.banto_seat), &id, AskVerdict::Deny, None).expect("the reviewer denies");
        kernel.settle_refusal_answer(&id, false).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(turns.try_recv().is_none(), "a deny requests no turn");
        assert!(kernel.refusal_hold(h.lane).is_err(), "the context stays held");
        assert!(kernel.kernel_db().lock().ask_redeemed(&id).unwrap(), "the answer is spent");
        kernel.shutdown_runtime_worker().await.unwrap();
    }

    /// An answer that arrives after a person's prompt already started a newer
    /// turn is spent without sending the work a second time.
    #[tokio::test]
    async fn an_allow_after_a_newer_turn_requests_nothing() {
        let h = house().await;
        let kernel = h.d.kernel();
        refuse(kernel, h.lane, h.coder, 1_000);
        let id = kernel.open_refusal_ask(h.lane).unwrap().unwrap();
        crate::kj::ledger::decide_ask(kernel, h.banto, Some(h.banto_seat), &id, AskVerdict::Allow, None).unwrap();
        {
            let db = kernel.kernel_db().lock();
            let turn_id = kaijutsu_types::TurnId::new();
            db.begin_turn(&crate::TurnStart { turn_id, context_id: h.lane, performer: h.coder, role: "model".into(),
                provider: "mock".into(), model: "mock-model".into(), started_at: 5_000 }).unwrap();
        }
        let mut turns = requested(kernel).await;
        kernel.settle_refusal_answer(&id, true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(turns.try_recv().is_none());
        assert!(kernel.kernel_db().lock().ask_redeemed(&id).unwrap());
        kernel.shutdown_runtime_worker().await.unwrap();
    }
}
