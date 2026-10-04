//! A council report stops an autonomous seat (`docs/council.md`, "The
//! flow": "Report stops an autonomous seat").
//!
//! [`stop_if_autonomous`] runs after a report opens its ask. The seat is
//! autonomous when the kernel drove its running turn
//! (`TurnOrigin::Autonomous`, [`crate::runtime::turn_state::TurnState::any_autonomous`])
//! or when the requester is not a live root character
//! ([`crate::kernel_db::CharacterRow::is_live_root`]), so a swarm seat driven
//! over ACP counts. A person at the keyboard keeps the ordinary ask.
//!
//! An autonomous seat is stopped through
//! [`crate::Kernel::interrupt_context_keeping_asks`]:
//!
//! - **Soft, not immediate.** The interrupt ends the reported call's wait
//!   on its ask, and the turn makes no further model call. A soft interrupt
//!   lets the other tool calls of the same round finish; each passed its own
//!   gate. An immediate one would cancel those commands part way through.
//! - **The ask stays redeemable.** Unlike `kj interrupt`, which abandons the
//!   asks the turn holds, this interrupt leaves them pending. An allow lets
//!   the approval worker run the stored command once and settle the call's
//!   result in place, as for an ask whose turn ended at the gate; a deny
//!   runs nothing. The seat stays stopped until someone drives it again.
//! - **By the system principal.** The interrupt is recorded on the context's
//!   continuation as the interrupter. No person or seat asked for this
//!   interrupt, so the record names the kernel (`PrincipalId::system()`),
//!   not the requester or the reviewer.

use kaijutsu_types::{ContextId, PrincipalId};

/// What [`stop_if_autonomous`] did, for the decision record and the report
/// event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReportStop {
    /// An interactive turn by a live root character, or no turn and a live
    /// root requester: the ordinary ask stands and nothing was interrupted.
    NotAutonomous,
    /// The seat is autonomous, but it had no accepted turn and no open
    /// continuation to stop.
    NothingRunning,
    /// The seat is autonomous and was interrupted.
    Interrupted {
        /// An accepted turn was told to stop.
        turn: bool,
        /// The open continuation was closed to automatic resume.
        continuation: bool,
    },
}

impl ReportStop {
    /// One word for a log field or span attribute.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NotAutonomous => "not_autonomous",
            Self::NothingRunning => "nothing_running",
            Self::Interrupted { .. } => "interrupted",
        }
    }
}

/// Stop `context`'s seat when it is autonomous. `requester` is the principal
/// that asked for the submission's turn (`KjCaller::principal_id`). `Err`
/// when the requester's character sheet or the interrupt record cannot be
/// written or read; the caller records it, and the ask stands either way.
pub(crate) fn stop_if_autonomous(
    kernel: &crate::Kernel,
    context: ContextId,
    requester: PrincipalId,
) -> Result<ReportStop, String> {
    let driven = kernel.turns().any_autonomous(context);
    let person = kernel
        .kernel_db()
        .lock()
        .get_character(requester)
        .map_err(|e| format!("could not read the requester's character sheet: {e}"))?
        .is_some_and(|sheet| sheet.is_live_root());
    if !driven && person {
        return Ok(ReportStop::NotAutonomous);
    }
    let outcome = kernel.interrupt_context_keeping_asks(context, PrincipalId::system())?;
    Ok(match (outcome.turn_interrupted, outcome.continuation_closed) {
        (false, false) => ReportStop::NothingRunning,
        (turn, continuation) => ReportStop::Interrupted { turn, continuation },
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::super::projection::fixtures::{character, live_context};
    use super::*;
    use crate::Kernel;
    use crate::flows::TurnOrigin;

    #[tokio::test]
    async fn an_autonomous_turn_is_interrupted_softly() {
        let kernel = Kernel::new_ephemeral("report-autonomous").await;
        let amy = character(&kernel, "amy", true);
        let seat = live_context(&kernel, "seat");
        let lease = kernel.turns().begin(seat);
        lease.set_origin(TurnOrigin::Autonomous);
        let stop = stop_if_autonomous(&kernel, seat, amy).unwrap();
        assert_eq!(stop, ReportStop::Interrupted { turn: true, continuation: false });
        let interrupt = lease.interrupt();
        assert!(interrupt.stop_after_turn.load(Ordering::Relaxed), "soft: stop before the next model call");
        assert!(interrupt.stop_waiting.is_cancelled(), "the reported call stops waiting");
        assert!(!interrupt.cancel.is_cancelled(), "not immediate: sibling calls finish");
        assert!(interrupt.keeps_held_asks(), "the reported call's ask stays pending");
    }

    #[tokio::test]
    async fn an_interactive_turn_by_a_root_keeps_the_ordinary_ask() {
        let kernel = Kernel::new_ephemeral("report-interactive").await;
        let amy = character(&kernel, "amy", true);
        let seat = live_context(&kernel, "seat");
        let lease = kernel.turns().begin(seat);
        lease.set_origin(TurnOrigin::Interactive);
        assert_eq!(stop_if_autonomous(&kernel, seat, amy).unwrap(), ReportStop::NotAutonomous);
        let interrupt = lease.interrupt();
        assert!(!interrupt.stop_after_turn.load(Ordering::Relaxed));
        assert!(!interrupt.stop_waiting.is_cancelled());
        assert!(!interrupt.keeps_held_asks());
    }

    /// `kj interrupt` and every other interrupt still abandon the asks a
    /// turn holds.
    #[tokio::test]
    async fn a_plain_interrupt_does_not_keep_held_asks() {
        let kernel = Kernel::new_ephemeral("report-plain").await;
        let seat = live_context(&kernel, "seat");
        for immediate in [false, true] {
            let lease = kernel.turns().begin(seat);
            kernel.interrupt_context(seat, immediate, PrincipalId::new()).unwrap();
            let interrupt = lease.interrupt();
            assert!(interrupt.stop_waiting.is_cancelled());
            assert!(!interrupt.keeps_held_asks(), "immediate={immediate}");
        }
    }

    #[tokio::test]
    async fn a_model_requester_counts_as_autonomous() {
        let kernel = Kernel::new_ephemeral("report-model").await;
        let coder = character(&kernel, "coder", false);
        let seat = live_context(&kernel, "seat");
        let lease = kernel.turns().begin(seat);
        lease.set_origin(TurnOrigin::Interactive);
        assert_eq!(
            stop_if_autonomous(&kernel, seat, coder).unwrap(),
            ReportStop::Interrupted { turn: true, continuation: false }
        );
        assert!(lease.interrupt().stop_after_turn.load(Ordering::Relaxed));

        let unsheeted = live_context(&kernel, "acp-seat");
        let _acp = kernel.turns().begin(unsheeted);
        assert!(
            matches!(stop_if_autonomous(&kernel, unsheeted, PrincipalId::new()).unwrap(), ReportStop::Interrupted { .. }),
            "a principal with no sheet is not a person"
        );
    }

    #[tokio::test]
    async fn a_retired_root_is_not_a_person_and_an_idle_seat_reports_nothing_running() {
        let kernel = Kernel::new_ephemeral("report-idle").await;
        let amy = character(&kernel, "amy", true);
        let seat = live_context(&kernel, "seat");
        assert_eq!(stop_if_autonomous(&kernel, seat, amy).unwrap(), ReportStop::NotAutonomous);
        assert!(kernel.kernel_db().lock().retire_character(amy, 1).unwrap());
        assert_eq!(stop_if_autonomous(&kernel, seat, amy).unwrap(), ReportStop::NothingRunning);
    }
}
