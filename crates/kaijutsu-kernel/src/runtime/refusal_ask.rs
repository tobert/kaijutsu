//! The ask a refused turn leaves for its reviewer.
//!
//! A provider's refusal stops the turn and holds the context
//! ([`Kernel::refusal_hold`]). The ask makes that stop a decision a person
//! sees: an allow lifts the hold and sends the context's work again, and a
//! deny leaves it held. The ask holds no source and runs nothing.

use std::sync::Arc;
use kaijutsu_types::{ContextId, PrincipalId, TurnId};
use crate::Kernel;

impl Kernel {
    /// Open the ask for `turn`, the context's turn the provider refused (its
    /// record already says `refused`), addressed to the reviewer the senpai
    /// chain gives that context. Returns the new ask's id, or `None` when an
    /// ask for this turn is still open or answered and not yet settled.
    pub(crate) fn open_refusal_ask(&self, context: ContextId, turn: TurnId) -> Result<Option<String>, String> {
        use approval_ledger::types::{NewAsk, NewOption, Origin};
        let request_id = {
            let db = self.kernel_db().lock();
            let open = approval_ledger::ask::list_unsettled_asks(db.conn_for_ledger(), context.as_bytes(), Origin::Refusal)
                .map_err(|e| format!("could not read the context's refusal asks: {e}"))?;
            for request in &open {
                let about = db.approval_refusal_turn(request)
                    .map_err(|e| format!("could not read the refused turn of ask {request}: {e}"))?;
                if about == Some(turn) {
                    return Ok(None);
                }
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
            db.create_approval_ask_recorded(&ask, false, |conn, request| {
                crate::kernel_db::record_approval_refusal_turn(conn, request, turn)
            })
                .map_err(|e| format!("could not record the refusal ask: {e}"))?
        };
        crate::kj::gate::announce_ledger_change(self.kernel_db(), self.ledger_flows());
        Ok(Some(request_id))
    }

    /// Withdraw the context's open refusal ask, because a newer turn replaced
    /// the refusal it asked about, and tell the ledger's watchers.
    pub(crate) fn withdraw_refusal_asks(&self, context: ContextId) -> Result<usize, String> {
        let withdrawn = self.kernel_db().lock().withdraw_refusal_asks(context)
            .map_err(|e| format!("could not withdraw the refusal ask: {e}"))?;
        if withdrawn > 0 {
            crate::kj::gate::announce_ledger_change(self.kernel_db(), self.ledger_flows());
        }
        Ok(withdrawn)
    }

    /// Act on an answered refusal ask, once. The slot and admission an allow
    /// needs are taken first, as `kj drive` does. The answer is spent and, for
    /// an allow, the refused turn's hold is lifted in one transaction, so a
    /// repeat finds the answer spent. An allow then requests an autonomous
    /// turn, which opens a new continuation epoch. It lifts nothing, and
    /// requests nothing, when a newer turn replaced the refusal the ask was
    /// about. A turn that cannot start leaves an error block in the context.
    pub(crate) fn settle_refusal_answer(self: &Arc<Self>, request_id: &str, allowed: bool) -> Result<(), String> {
        let row = self.kernel_db().lock().get_approval(request_id)
            .map_err(|e| format!("could not read the refusal ask: {e}"))?
            .ok_or_else(|| format!("refusal ask {request_id} has no row"))?;
        let context = ContextId::try_from_slice(&row.context_id)
            .ok_or_else(|| format!("refusal ask {request_id} has no context"))?;
        let turn = self.kernel_db().lock().approval_refusal_turn(request_id)
            .map_err(|e| format!("could not read the refusal ask's turn: {e}"))?
            .ok_or_else(|| format!("refusal ask {request_id} records no turn"))?;
        let reserved = if allowed {
            self.reserve_runtime_slot().and_then(|slot| self.admit_context(context).map(|admission| (slot, admission)))
        } else {
            Err("a deny resumes nothing".into())
        };
        let (lift, reserved) = match reserved {
            Ok(pair) => (true, Some(pair)),
            Err(_) if !allowed => (false, None),
            Err(why) => {
                self.spend_refusal_answer(request_id, context, turn, false)?;
                let error = format!("the resume did not start and the context is still held: {why}");
                self.note_refusal_resume_failure(context, &error);
                return Err(error);
            }
        };
        if !self.spend_refusal_answer(request_id, context, turn, lift)? {
            return Ok(());
        }
        let Some((slot, admission)) = reserved else { return Ok(()) };
        let principal = row.decided_by.as_deref().and_then(PrincipalId::try_from_slice)
            .or_else(|| row.actor_id.as_deref().and_then(PrincipalId::try_from_slice))
            .ok_or_else(|| format!("refusal ask {request_id} names nobody to resume as"))?;
        let started = match self.blocks().last_block_id(context) {
            Some(after) => self.request_turn_admitted(super::turn_request::TurnRequest {
                context_id: context, after_block_id: after, content: String::new(),
                principal_id: principal, continuation_epoch: None, score: None,
            }, slot, admission).map(|_| ()),
            None => Err(format!("context {} has no blocks to anchor a turn after", context.short())),
        };
        started.map_err(|why| {
            let error = format!("the resume did not start; the hold is lifted, so a prompt or drive can continue it: {why}");
            self.note_refusal_resume_failure(context, &error);
            error
        })
    }

    /// Spend the answer and, when `lift`, lift the refused turn's hold, in one
    /// transaction. Returns whether the context should now be resumed: this
    /// call spent the answer and the turn was still the one holding it.
    fn spend_refusal_answer(&self, request_id: &str, context: ContextId, turn: TurnId, lift: bool) -> Result<bool, String> {
        let resumed = self.kernel_db().lock().in_transaction(|db| {
            if !db.redeem_ask(request_id)? {
                return Ok(false);
            }
            Ok(lift && db.release_refusal_hold(context, turn)?)
        }).map_err(|e| format!("could not spend the refusal ask's answer: {e}"))?;
        if lift && !resumed {
            tracing::info!(%context, ask = request_id, "refusal ask settled with nothing to lift: a newer turn replaced the refusal or the answer was already spent");
        }
        crate::kj::gate::announce_ledger_change(self.kernel_db(), self.ledger_flows());
        Ok(resumed)
    }

    /// Say in the context's log that an allowed resume did not start.
    fn note_refusal_resume_failure(&self, context: ContextId, error: &str) {
        tracing::error!(%context, %error, "refusal resume");
        let payload = kaijutsu_types::ErrorPayload {
            category: kaijutsu_types::ErrorCategory::Stream, severity: kaijutsu_types::ErrorSeverity::Error,
            code: None, detail: Some(error.to_string()), span: None, source_kind: None,
        };
        let written = self.blocks().last_block_id(context).ok_or_else(|| "the context has no blocks".to_string())
            .and_then(|after| self.blocks().insert_error_block_as(context, &after, &payload, payload.summary_line(),
                Some(PrincipalId::system())).map_err(|e| e.to_string()));
        if let Err(write) = written {
            tracing::error!(%context, %write, "could not record that the refusal resume did not start");
        }
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
    fn refuse(kernel: &Kernel, context: ContextId, performer: PrincipalId, at: i64) -> TurnId {
        let turn_id = kaijutsu_types::TurnId::new();
        let db = kernel.kernel_db().lock();
        db.begin_turn(&crate::TurnStart { turn_id, context_id: context, performer, role: "model".into(),
            provider: "mock".into(), model: "mock-model".into(), started_at: at }).unwrap();
        db.finish_turn(turn_id, at + 1, "refused").unwrap();
        turn_id
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
        let turn = refuse(kernel, h.lane, h.coder, 1_000);
        let id = kernel.open_refusal_ask(h.lane, turn).unwrap().expect("a refusal opens an ask");
        let asks = refusal_asks(kernel, h.lane);
        let [ask] = &asks[..] else { panic!("one ask: {asks:?}") };
        assert_eq!(ask.request_id, id);
        assert!(ask.exec_source.is_none() && ask.hook_id.is_none(), "it carries no source");
        let text = &ask.description;
        for needed in ["lane", "declined", "approving", "again", "kj context rotate"] {
            assert!(text.to_lowercase().contains(needed), "the ask says {needed:?}: {text}");
        }
        assert_eq!(ask.reviewer_id.as_deref(), Some(h.banto.as_bytes().as_slice()), "a kohai's reviewer is its senpai's performer");

        assert_eq!(kernel.open_refusal_ask(h.lane, turn).unwrap(), None, "a second refusal does not stack another ask");
        assert_eq!(refusal_asks(kernel, h.lane).len(), 1);

        let turn = refuse(kernel, h.banto_seat, h.banto, 2_000);
        kernel.open_refusal_ask(h.banto_seat, turn).unwrap().expect("the seat's refusal opens its own ask");
        let seat_asks = refusal_asks(kernel, h.banto_seat);
        assert_eq!(seat_asks[0].reviewer_id.as_deref(), Some(h.amy.as_bytes().as_slice()), "a senpai's reviewer is the person");
    }

    /// `kj ledger list` and `show` render the ask by its origin.
    #[tokio::test]
    async fn the_ledger_lists_and_shows_a_refusal_ask() {
        let h = house().await;
        let turn = refuse(h.d.kernel(), h.lane, h.coder, 1_000);
        let id = h.d.kernel().open_refusal_ask(h.lane, turn).unwrap().unwrap();
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
        let turn = refuse(kernel, h.lane, h.coder, 1_000);
        assert!(kernel.refusal_hold(h.lane).is_err(), "the context is held");
        let id = kernel.open_refusal_ask(h.lane, turn).unwrap().unwrap();
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
        let turn = refuse(kernel, h.lane, h.coder, 1_000);
        let id = kernel.open_refusal_ask(h.lane, turn).unwrap().unwrap();
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
        let turn = refuse(kernel, h.lane, h.coder, 1_000);
        let id = kernel.open_refusal_ask(h.lane, turn).unwrap().unwrap();
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

    /// An answer is about the turn the ask was opened for. When a newer turn
    /// was refused meanwhile, an earlier allow or deny touches nothing of it,
    /// and the newer refusal still gets its own ask.
    #[tokio::test]
    async fn an_answer_to_an_earlier_refusal_does_not_decide_a_later_one() {
        for verdict in [AskVerdict::Allow, AskVerdict::Deny] {
            let h = house().await;
            let kernel = h.d.kernel();
            let first = refuse(kernel, h.lane, h.coder, 1_000);
            let a1 = kernel.open_refusal_ask(h.lane, first).unwrap().unwrap();
            crate::kj::ledger::decide_ask(kernel, h.banto, Some(h.banto_seat), &a1, verdict, None).unwrap();
            // A person prompts; that turn is refused too, before the worker settles A1.
            let second = refuse(kernel, h.lane, h.coder, 5_000);
            let a2 = kernel.open_refusal_ask(h.lane, second).unwrap()
                .expect("the later refusal gets its own ask while the earlier answer is unsettled");
            assert_ne!(a1, a2);

            let mut turns = requested(kernel).await;
            kernel.settle_refusal_answer(&a1, verdict == AskVerdict::Allow).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            assert!(turns.try_recv().is_none(), "{verdict:?}: the earlier answer requests no turn");
            assert!(kernel.refusal_hold(h.lane).is_err(), "{verdict:?}: the later refusal is still held");
            assert_eq!(refusal_asks(kernel, h.lane).len(), 1, "{verdict:?}: the later ask is still open");
            kernel.shutdown_runtime_worker().await.unwrap();
        }
    }

    /// A newer turn withdraws the pending ask, and the ledger's watchers hear
    /// of it; with nothing pending nothing is announced.
    #[tokio::test]
    async fn withdrawing_a_pending_ask_tells_the_ledger_watchers() {
        let h = house().await;
        let kernel = h.d.kernel();
        let turn = refuse(kernel, h.lane, h.coder, 1_000);
        let id = kernel.open_refusal_ask(h.lane, turn).unwrap().unwrap();
        let mut changed = kernel.ledger_flows().subscribe("ledger.changed");
        while changed.try_recv().is_some() {}

        assert_eq!(kernel.withdraw_refusal_asks(h.lane).unwrap(), 1);
        assert!(refusal_asks(kernel, h.lane).is_empty(), "the ask is no longer pending");
        let row = kernel.kernel_db().lock().get_approval(&id).unwrap().unwrap();
        assert_eq!(row.status, crate::ApprovalStatus::Abandoned);
        tokio::time::timeout(std::time::Duration::from_secs(2), changed.recv()).await
            .expect("watchers are told the ask is gone").unwrap();

        assert_eq!(kernel.withdraw_refusal_asks(h.lane).unwrap(), 0);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(changed.try_recv().is_none(), "nothing withdrawn, nothing announced");
        kernel.shutdown_runtime_worker().await.unwrap();
    }

    /// An allow whose turn cannot start leaves a visible error in the context
    /// and keeps the hold, rather than doing nothing silently.
    #[tokio::test]
    async fn an_allow_that_cannot_start_a_turn_says_so_in_the_context() {
        let h = house().await;
        let kernel = h.d.kernel();
        let turn = refuse(kernel, h.lane, h.coder, 1_000);
        let id = kernel.open_refusal_ask(h.lane, turn).unwrap().unwrap();
        crate::kj::ledger::decide_ask(kernel, h.banto, Some(h.banto_seat), &id, AskVerdict::Allow, None).unwrap();
        kernel.shutdown_runtime_worker().await.unwrap();

        let error = kernel.settle_refusal_answer(&id, true).expect_err("no runtime slot, no turn");
        assert!(error.contains("did not start"), "{error}");
        let blocks = kernel.blocks().block_snapshots(h.lane).unwrap();
        let note = blocks.iter().find(|b| b.kind == kaijutsu_types::BlockKind::Error).expect("an error block says so");
        assert!(note.content.contains("did not start"), "{}", note.content);
        assert!(kernel.refusal_hold(h.lane).is_err(), "the hold stays when nothing resumed");
        assert!(kernel.kernel_db().lock().ask_redeemed(&id).unwrap(), "the answer is spent once");
    }

    /// The approval worker, not a hand call, settles an allow: it resumes the
    /// refused context once.
    #[tokio::test]
    async fn the_approval_worker_resumes_a_refused_context_on_an_allow() {
        let h = house().await;
        let kernel = h.d.kernel();
        let turn = refuse(kernel, h.lane, h.coder, 1_000);
        let id = kernel.open_refusal_ask(h.lane, turn).unwrap().unwrap();
        let mut turns = requested(kernel).await;
        kernel.start_approval_delivery().unwrap();
        crate::kj::ledger::decide_ask(kernel, h.banto, Some(h.banto_seat), &id, AskVerdict::Allow, None).unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), turns.recv()).await
            .expect("the worker settles the allow and requests a turn").unwrap();
        assert!(matches!(&event.payload, crate::flows::TurnFlow::Requested { context_id, .. } if *context_id == h.lane), "{:?}", event.payload);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(turns.try_recv().is_none(), "the worker requests the turn once");
        assert!(kernel.kernel_db().lock().ask_redeemed(&id).unwrap());
        kernel.shutdown_runtime_worker().await.unwrap();
    }
}
