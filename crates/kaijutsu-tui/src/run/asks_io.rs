//! The kernel reads and answers an ask needs, run on their own tasks and
//! landed through the loop.

use kaijutsu_client::ActorHandle;
use tokio::sync::mpsc;

use crate::app::App;
use crate::asks;
use crate::bridge::KernelBridge;

/// Why an ask's record was read off the loop ([`spawn_ask_read`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AskReadFor {
    /// The card candidate's record, to put its card up.
    Card,
    /// A card that came down because its ask closed elsewhere: the decision
    /// for its notice ([`asks::answered_notice`]).
    Answered,
}

/// Kernel work about asks, landed on the loop's `ask_landed` arm.
pub(super) enum AskLanded {
    Read {
        request_id: String,
        purpose: AskReadFor,
        result: std::result::Result<Option<kaijutsu_types::AskDetail>, kaijutsu_client::actor::CallError>,
    },
    Ledger(std::result::Result<asks::LedgerReads, kaijutsu_client::actor::CallError>),
    Shown {
        request_id: String,
        result: std::result::Result<Option<kaijutsu_types::AskDetail>, kaijutsu_client::actor::CallError>,
    },
    Decided {
        request_id: String,
        answer: std::result::Result<kaijutsu_client::AskAnswer, kaijutsu_client::actor::CallError>,
    },
    Escalated {
        request_id: String,
        took: bool,
        answer: std::result::Result<kaijutsu_client::AskAnswer, kaijutsu_client::actor::CallError>,
    },
}

/// Read one ask's record on its own task and land it on the loop's
/// `ask_landed` arm, so no key waits on the kernel for it.
pub(super) fn spawn_ask_read(
    bridge: &KernelBridge,
    tx: &mpsc::UnboundedSender<AskLanded>,
    request_id: String,
    purpose: AskReadFor,
) {
    let actor = bridge.actor().clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = actor.get_ask(request_id.clone()).await;
        // The loop is gone when this fails; nothing is left to show it.
        let _ = tx.send(AskLanded::Read { request_id, purpose, result });
    });
}

/// Run each item of kernel work a key asked for ([`App::ask_work`]) on its
/// own task, landing on the loop's `ask_landed` arm.
pub(super) fn spawn_ask_work(bridge: &KernelBridge, app: &mut App, tx: &mpsc::UnboundedSender<AskLanded>) {
    for work in std::mem::take(&mut app.ask_work) {
        let actor = bridge.actor().clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let landed = match work {
                asks::AskWork::OpenLedger { open } => AskLanded::Ledger(read_ledger(&actor, open).await),
                asks::AskWork::Show(request_id) => {
                    let result = actor.get_ask(request_id.clone()).await;
                    AskLanded::Shown { request_id, result }
                }
                asks::AskWork::Decide { request_id, verdict, remember } => {
                    let answer = actor.decide_ask(request_id.clone(), verdict, remember).await;
                    AskLanded::Decided { request_id, answer }
                }
                asks::AskWork::Escalate { request_id, to } => {
                    let took = to.is_some();
                    let answer = actor.escalate_ask(request_id.clone(), to).await;
                    AskLanded::Escalated { request_id, took, answer }
                }
            };
            // The loop is gone when this fails; nothing is left to show it.
            let _ = tx.send(landed);
        });
    }
}

/// The ledger view's records: the recent answers listed, then every record
/// (the open asks named, then the answers) read concurrently.
async fn read_ledger(
    actor: &ActorHandle,
    open: Vec<String>,
) -> std::result::Result<asks::LedgerReads, kaijutsu_client::actor::CallError> {
    let history = kaijutsu_types::AskFilter {
        view: kaijutsu_types::AskView::History,
        limit: Some(asks::LEDGER_HISTORY_LIMIT),
        ..Default::default()
    };
    let answered = actor.list_asks(history).await?.asks;
    let ids: Vec<String> = open.into_iter().chain(answered.into_iter().map(|ask| ask.request_id)).collect();
    let reads: Vec<_> = ids
        .iter()
        .map(|id| {
            let actor = actor.clone();
            let id = id.clone();
            tokio::spawn(async move { actor.get_ask(id).await })
        })
        .collect();
    let mut landed = Vec::with_capacity(ids.len());
    for (id, read) in ids.into_iter().zip(reads) {
        let result = read.await.unwrap_or_else(|join| panic!("a ledger read task failed: {join}"));
        landed.push((id, result));
    }
    Ok(landed)
}

/// Read the card candidate's record when there is a candidate and no read
/// for it is in flight. The card goes up when the read lands, if the
/// candidate is still the same ask ([`asks::open_card`]).
pub(super) fn raise_card(
    bridge: &KernelBridge,
    app: &App,
    tx: &mpsc::UnboundedSender<AskLanded>,
    in_flight: &mut Option<String>,
) {
    if in_flight.is_some() {
        return;
    }
    if let Some(request_id) = asks::card_candidate(app) {
        *in_flight = Some(request_id.clone());
        spawn_ask_read(bridge, tx, request_id, AskReadFor::Card);
    }
}
