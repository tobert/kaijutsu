//! Command completion is published only after result hooks settle.

mod common;
use common::*;
use std::sync::Arc;
use kaijutsu_kernel::mcp::{CallContext, Hook, HookAction, HookBody, HookEntry, HookId, KernelCallParams, McpResult};
use kaijutsu_types::Status;

struct PausedHook {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl Hook for PausedHook {
    async fn invoke(&self, _: &KernelCallParams, _: &CallContext) -> McpResult<()> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

#[test]
fn structured_kj_blocks_wait_for_post_call() {
    run_local(async {
        let (addr, kernel) = start_server_with_kernel_handle().await;
        let client = connect_client(addr).await;
        let (kj, _) = client.bind_kernel().await.unwrap();
        let contexts = kj.list_contexts().await.unwrap();
        let context = kaijutsu_client::choose_parent(None, &contexts).unwrap().context_id;
        let before: std::collections::HashSet<_> = kernel.documents.block_snapshots(context)
            .unwrap().into_iter().map(|block| block.id).collect();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        kernel.kernel.broker().hooks().write().await.post_call.entries.push(HookEntry {
            id: HookId("wait-for-post-call".into()), match_instance: None, match_tool: None,
            match_context: Some(context), match_principal: None,
            action: HookAction::Invoke(HookBody::Builtin { name: "pause".into(),
                hook: Arc::new(PausedHook { entered: entered.clone(), release: release.clone() }) }),
            priority: 0, kaish_script_id: None,
        });
        let argv = ["context".into(), "current".into()];
        let execute = kj.execute_kj(context, &argv);
        let observe = async {
            tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified()).await.unwrap();
            let statuses: Vec<_> = kernel.documents.block_snapshots(context).unwrap().into_iter()
                .filter(|block| !before.contains(&block.id)).map(|block| block.status).collect();
            release.notify_one();
            statuses
        };
        let (result, statuses) = tokio::join!(execute, observe);
        assert_eq!(result.unwrap().exit_code, 0);
        assert_eq!(statuses, vec![Status::Running, Status::Running]);
        let final_statuses: Vec<_> = kernel.documents.block_snapshots(context).unwrap().into_iter()
            .filter(|block| !before.contains(&block.id)).map(|block| block.status).collect();
        assert_eq!(final_statuses, vec![Status::Done, Status::Done]);
    });
}
