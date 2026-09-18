//! Context execution admission, ordered against archive by the kernel database.
//!
//! Admission accepts one request. Queueing and asynchronous preparation may still
//! fail; archive cannot revoke an accepted request or its settlement destination.
//! This proof does not replace command capabilities, hooks, or approval policy.

use kaijutsu_types::ContextId;
use crate::{Kernel, kernel_db::{ContextRow, KernelDb}};

/// One accepted request in a context. Keep this proof through preparation instead
/// of checking archive again after the request has already been accepted.
#[derive(Debug)]
pub struct ContextAdmission {
    context: ContextId,
}

impl ContextAdmission {
    pub fn context(&self) -> ContextId { self.context }

    /// Mint admission for a row that was successfully inserted while the caller
    /// retains the database guard. The known row avoids a second fallible read;
    /// retaining the guard keeps archive ordered after this acceptance point.
    pub(crate) fn for_inserted(row: &ContextRow) -> Self {
        assert!(!row.is_archived(), "an inserted archived context cannot admit work");
        Self { context: row.context_id }
    }

    pub(crate) fn acquire(db: &KernelDb, context: ContextId) -> Result<Self, String> {
        let row = db.get_context(context).map_err(|error| format!("could not admit execution in context {context}: {error}"))?
            .ok_or_else(|| format!("context {context} not found; no execution was admitted"))?;
        if row.is_archived() {
            return Err(format!("context {context} is archived; restore it with `kj context promote {context}` before starting new work"));
        }
        Ok(Self { context })
    }
}

impl Kernel {
    /// Accept one request before its first mutation or asynchronous preparation.
    /// The database guard orders this decision against context archive. The
    /// accepted request may finish even if its context is archived afterward.
    pub fn admit_context(&self, context: ContextId) -> Result<ContextAdmission, String> {
        ContextAdmission::acquire(&self.kernel_db().lock(), context)
    }

    /// Accept and queue one context request on the existing runtime worker.
    /// The task carries admission through preparation; shutdown owns cancellation.
    pub(crate) fn spawn_context_task<F, W>(&self, context: ContextId, work: W) -> Result<(), String>
    where F: std::future::Future<Output = ()> + 'static,
        W: FnOnce(ContextAdmission, tokio_util::sync::CancellationToken) -> F + Send + 'static,
    {
        let db = self.kernel_db().lock();
        let admission = ContextAdmission::acquire(&db, context)?;
        self.spawn_runtime_task(move |stop| work(admission, stop))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use kaijutsu_types::{PrincipalId, SessionId};
    use crate::kj::test_helpers::{register_context, test_dispatcher_persistent};
    use crate::runtime::context_shell::ShellIdentity;

    #[tokio::test]
    async fn accepted_shell_preparation_survives_archive_before_worker_entry() {
        for path in ["interactive", "structured", "quiet", "streaming", "editor", "dry-run"] {
            let dispatcher = Arc::new(test_dispatcher_persistent().await);
            dispatcher.set_self_arc();
            let kernel = dispatcher.kernel();
            kernel.broker().set_kj_dispatcher(&dispatcher).await;
            let principal = PrincipalId::new();
            let context = register_context(&dispatcher, Some("admitted-before-archive"), None, principal);
            kernel.blocks().create_document(context, crate::DocumentKind::Conversation, None).unwrap();
            let identity = ShellIdentity { requester: principal, performer: principal, reviewer: None,
                context, session: SessionId::new() };
            let (entered, ready) = tokio::sync::oneshot::channel();
            let (release, held) = std::sync::mpsc::channel();
            kernel.spawn_runtime_task(move |_| async move {
                entered.send(()).unwrap();
                held.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
            }).unwrap();
            ready.await.unwrap();
            let request = async {
                match path {
                    "interactive" => {
                        let (submission, _) = crate::runtime::interactive::submit(kernel, identity,
                            crate::runtime::interactive::ShellSource::Code("echo admitted-before-archive".into()), true).await.unwrap();
                        tokio::time::timeout(std::time::Duration::from_secs(5), async {
                            loop {
                                let state = kernel.shell_operations().get(&submission.operation_id, context).unwrap().unwrap();
                                if let Some(envelope) = state.envelope { break envelope.stdout; }
                                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                            }
                        }).await.unwrap()
                    }
                    "structured" | "quiet" => crate::runtime::structured::execute_kj(kernel, identity,
                        &["context".into(), "current".into()], path == "quiet").await.unwrap().unwrap().stdout,
                    "streaming" => crate::runtime::streaming::execute(kernel, identity,
                        "echo admitted-before-archive".into(), tokio_util::sync::CancellationToken::new())
                        .await.unwrap().unwrap().completed.await.unwrap().unwrap().envelope().stdout,
                    "editor" => crate::runtime::editor_read::read_shell(dispatcher.clone(), identity,
                        "echo admitted-before-archive".into()).await.unwrap(),
                    "dry-run" => {
                        let call = crate::mcp::CallContext::new(principal, context, identity.session, kernel.id());
                        let report = crate::runtime::dry_run::inspect_shell(kernel, call,
                            "echo admitted-before-archive".into()).await.unwrap();
                        assert!(matches!(report.outcome, crate::mcp::DryRunOutcome::WouldProceed));
                        "admitted-before-archive".into()
                    }
                    _ => unreachable!(),
                }
            };
            tokio::pin!(request);
            assert!(futures::poll!(&mut request).is_pending(), "admitted preparation waits for the worker");
            assert!(kernel.shell_operations().list_for_context(context).unwrap().is_empty(), "no receipt exists before preparation");
            kernel.kernel_db().lock().archive_context(context).unwrap();
            release.send(()).unwrap();
            let output = request.await;
            assert!(output.contains("admitted-before-archive"), "{path}: {output}");
            kernel.shutdown_runtime_worker().await.unwrap();
        }
    }

    #[tokio::test]
    async fn unreadable_context_cannot_queue_a_factory() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        let dispatcher = test_dispatcher_persistent().await;
        let kernel = dispatcher.kernel();
        let context = register_context(&dispatcher, Some("unreadable-admission"), None, PrincipalId::new());
        kernel.kernel_db().lock().conn_for_ledger().authorizer(Some(|auth: AuthContext<'_>| match auth.action {
            AuthAction::Read { table_name: "contexts", .. } => Authorization::Deny,
            _ => Authorization::Allow,
        })).unwrap();
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = ran.clone();
        let error = kernel.spawn_context_task(context, move |_, _| async move {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }).unwrap_err();
        kernel.kernel_db().lock().conn_for_ledger().authorizer(None::<fn(AuthContext<'_>) -> Authorization>).unwrap();
        kernel.shutdown_runtime_worker().await.unwrap();
        assert!(error.contains("could not admit"), "{error}");
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
        assert!(kernel.shell_operations().list_for_context(context).unwrap().is_empty());
    }
}
