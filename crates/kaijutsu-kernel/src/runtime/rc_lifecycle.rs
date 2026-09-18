//! Runtime ownership for lifecycle triggers emitted by the beat scheduler.

use std::collections::HashMap;
use std::sync::Arc;
use kaijutsu_types::{PrincipalId, SessionId};
use crate::{KjCaller, KjDispatcher};
use super::admission::ContextAdmission;

/// Queue an already accepted lifecycle with its captured transport variables.
/// Archive cannot revoke its admission. The runtime signals cancellation and
/// joins rc cleanup on shutdown; the clock thread never waits for execution.
pub fn submit(
    dispatcher: Arc<KjDispatcher>,
    admission: ContextAdmission,
    verb: &'static str,
    vars: HashMap<String, String>,
) -> Result<(), String> {
    let kernel = dispatcher.kernel().clone();
    kernel.spawn_runtime_task(move |stop| async move {
        let context = admission.context();
        // The clock invokes lifecycle policy as the system performer; it does
        // not impersonate the character assigned to the target context.
        let caller = KjCaller {
            principal_id: PrincipalId::system(),
            actor_id: PrincipalId::system(),
            reviewer_id: None,
            context_id: Some(context),
            session_id: SessionId::new(),
            confirmed: false,
            rc_depth: 0,
            privileged: false,
            cancel: stop.child_token(),
        };
        let invocation = crate::rc::RcInvocation {
            vars,
            ..crate::rc::RcInvocation::new(verb, &admission, &caller.cancel)
        };
        if let Err(error) = crate::rc::run(&dispatcher, invocation, &caller).await {
            tracing::warn!(context = %context, verb, "scheduled rc lifecycle: {error}");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::test_helpers::{install_rc_script_file, register_context, test_dispatcher_persistent};

    #[tokio::test]
    async fn queued_lifecycle_keeps_admission_and_transport_snapshot_after_archive() {
        let dispatcher = Arc::new(test_dispatcher_persistent().await);
        dispatcher.set_self_arc();
        let context = register_context(&dispatcher, Some("queued-tick"), None, PrincipalId::new());
        install_rc_script_file(&dispatcher, "/config/rc/default/tick/S00-snapshot.kai",
            "kj block create --role system --kind text --content $KJ_TICK").await;
        let kernel = dispatcher.kernel();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        kernel.spawn_runtime_task(move |_| async move {
            entered.send(()).unwrap();
            held.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        }).unwrap();
        ready.await.unwrap();
        let admission = kernel.admit_context(context).unwrap();
        submit(dispatcher.clone(), admission, "tick", HashMap::from([("KJ_TICK".into(), "42".into())])).unwrap();
        kernel.kernel_db().lock().archive_context(context).unwrap();
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let done = {
                    let db = kernel.kernel_db().lock();
                    approval_ledger::rc_runs::list_runs(db.conn_for_ledger()).unwrap().iter().any(|run|
                        run.context_id == context.as_bytes() && run.verb == "tick"
                            && run.outcome == Some(approval_ledger::types::RcOutcome::Ok))
                };
                if done { break; }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }).await.expect("queued admission must remain valid after archive");
        assert!(kernel.blocks().block_snapshots(context).unwrap().iter().any(|block| block.content == "42"));
        kernel.shutdown_runtime_worker().await.unwrap();
    }
}
