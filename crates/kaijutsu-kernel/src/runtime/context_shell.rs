//! Contextual kaish construction shared by command, rc, hook, and editor callers.
//!
//! Each invocation gets isolated scope and session tracking, seeded from durable
//! cwd and environment. Callers own write-back policy; `kj context set` writes
//! durable state explicitly. Kaish jobs share the context's job manager and can
//! outlive the invocation. See `docs/kaish-integration.md`.

use std::sync::Arc;

use anyhow::Result;
use kaijutsu_types::{ContextId, PrincipalId, SessionId};

use crate::kj::KjDispatcher;
use crate::rc::RcAuthority;
use super::context_engine::SessionContextMap;
use super::embedded_kaish::{EmbeddedKaish, OutputProfile};

/// Identity captured before execution; context switches do not change the performer.
#[derive(Clone, Copy, Debug)]
pub struct ShellIdentity {
    pub requester: PrincipalId,
    pub performer: PrincipalId,
    pub reviewer: Option<PrincipalId>,
    pub context: ContextId,
    pub session: SessionId,
}

/// Execution profiles distinguish output consumers from lifecycle authority.
pub enum ShellPolicy {
    /// Context loadout controls host execution; output uses the agent limit.
    Agent,
    /// Hook and editor output uses the internal limit, without rc authority.
    Internal,
    /// Shared file/state writes, host execution, MCP calls, and network tools
    /// are refused. Only classified `kj` reads and local shell operations run.
    ReadOnly,
    /// Only lifecycle orchestration can supply the authority to widen a loadout.
    Rc(RcAuthority),
}

/// Source of the working directory, independent of output and execution policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellCwd {
    /// Restore the current context's durable cwd; an unset cwd keeps the default.
    Context,
    /// Restore the approval's captured cwd, including its explicitly unset state.
    Captured(Option<std::path::PathBuf>),
}

/// Inputs shared by contextual construction and approval capture.
pub(crate) struct ContextShellInputs {
    pub cwd: Option<std::path::PathBuf>,
    pub exports: Vec<crate::kernel_db::ContextEnvRow>,
    pub external_exec: super::embedded_kaish::ExternalExec,
    /// The context's `context_egress` rows (`docs/egress.md`), handed to
    /// `curl_tool` so the shell's `curl` reaches only what this context's
    /// list names.
    pub egress_hosts: Vec<String>,
    /// The host of `[classifier] url` in `gate.toml`, handed to `curl_tool`
    /// beside `egress_hosts` — the one host every context reaches beyond its
    /// own rows (`docs/egress.md`, "The classifier host"). `None` with no
    /// `[classifier]` section or an unreadable/unparseable `gate.toml`: the
    /// gate itself already refuses every gated shell submission on a bad
    /// file, so shell construction does not fail a second time on it.
    pub classifier_host: Option<String>,
}

impl ContextShellInputs {
    pub async fn load(kernel: &crate::Kernel, context: ContextId, read_only: bool, cwd: ShellCwd) -> Result<Self> {
        let external_exec = if !read_only && kernel.broker().binding_checked(&context).await?
            .allows(&crate::mcp::Capability::Exec) {
            super::embedded_kaish::ExternalExec::Allow { path: kernel.host_path().map(str::to_string) }
        } else {
            super::embedded_kaish::ExternalExec::Deny
        };
        let (cwd, exports, egress_hosts) = {
            let db = kernel.kernel_db().lock();
            let cwd = match cwd {
                ShellCwd::Context => super::shell_state::read_context_cwd(&db, context)
                    .map_err(anyhow::Error::msg)?,
                ShellCwd::Captured(cwd) => cwd,
            };
            super::shell_state::validate_cwd(cwd.as_deref()).map_err(anyhow::Error::msg)?;
            let exports = db.get_context_env(context)
                .map_err(|error| anyhow::anyhow!("read context_env for {context}: {error}"))?;
            let egress_hosts = db.list_context_egress(context)
                .map_err(|error| anyhow::anyhow!("read context_egress for {context}: {error}"))?;
            (cwd, exports, egress_hosts)
        };
        let classifier_host = crate::kj::gate_policy::load_config(kernel.vfs())
            .await
            .ok()
            .and_then(|config| config.classifier().map(|c| c.host().to_string()));
        Ok(Self { cwd, exports, external_exec, egress_hosts, classifier_host })
    }

    pub fn environment(&self) -> std::collections::HashMap<String, String> {
        let mut env: std::collections::HashMap<_, _> = initial_environment(&self.external_exec)
            .into_iter().map(|(name, value)| (name, kaish_kernel::interpreter::value_to_string(&value))).collect();
        // Kaish initializes PWD from cwd before durable exports are applied.
        let cwd = self.cwd.clone().unwrap_or_else(kaish_kernel::home_dir);
        env.insert("PWD".into(), cwd.to_string_lossy().into_owned());
        env.extend(self.exports.iter().map(|row| (row.key.clone(), row.value.clone())));
        env
    }
}

/// Interpreter defaults. Durable exports may override these values.
pub(crate) fn initial_environment(external_exec: &super::embedded_kaish::ExternalExec)
    -> std::collections::HashMap<String, kaish_kernel::ast::Value>
{
    use kaish_kernel::ast::Value;
    let mut env = std::collections::HashMap::from([
        ("HOME".into(), Value::String(kaish_kernel::home_dir().to_string_lossy().into_owned())),
    ]);
    if let super::embedded_kaish::ExternalExec::Allow { path: Some(path) } = external_exec {
        env.insert("PATH".into(), Value::String(path.clone()));
    }
    env
}

impl EmbeddedKaish {
    /// Construct a contextual shell with explicit identity and execution policy.
    ///
    /// Synthesis uses the supplied index and block source. This constructor seeds
    /// scope but does not persist it or run command hooks.
    pub async fn for_context(
        dispatcher: &KjDispatcher,
        name: &str,
        identity: ShellIdentity,
        policy: ShellPolicy,
        cwd: ShellCwd,
        semantic_index: Option<Arc<kaijutsu_index::SemanticIndex>>,
        block_source: Arc<dyn kaijutsu_index::BlockSource>,
    ) -> Result<Self> {
        let ShellIdentity {
            requester: principal, context: context_id, session: session_id, ..
        } = identity;
        let (privileged, read_only, output) = match policy {
            ShellPolicy::Agent => (false, false, OutputProfile::Agent),
            ShellPolicy::Internal => (false, false, OutputProfile::Internal),
            ShellPolicy::ReadOnly => (false, true, OutputProfile::Agent),
            ShellPolicy::Rc(_) => (true, false, OutputProfile::Internal),
        };
        // Fresh, isolated session map: this kaish lives for one invocation and
        // tracks exactly one session→context mapping. No cross-invocation
        // leakage, nothing to evict.
        let session_contexts: SessionContextMap =
            crate::runtime::context_engine::session_context_map();
        session_contexts.insert(session_id, context_id);

        let kj_dispatcher = dispatcher.self_arc().ok_or_else(|| {
            anyhow::anyhow!("context shell requires a registered kj dispatcher")
        })?;
        // Loaded before `configure_tools` so the closure can move the
        // context's egress rows straight into `curl_tool` — the shell reads
        // them once, when it is built (`docs/egress.md`, "Who changes the
        // list").
        let inputs = ContextShellInputs::load(dispatcher.kernel(), context_id, read_only, cwd.clone()).await?;
        let egress_hosts = inputs.egress_hosts.clone();
        let classifier_host = inputs.classifier_host.clone();
        let configure_tools =
            move |scm: SessionContextMap,
                  _sid: SessionId,
                  tools: &mut kaish_kernel::ToolRegistry| {
                let d = kj_dispatcher;
                tools.register(crate::runtime::vi_builtin::ViBuiltin::new(
                    d.clone(), "vi", identity, scm.clone(),
                ));
                tools.register(crate::runtime::vi_builtin::ViBuiltin::new(
                    d.clone(), "edit", identity, scm.clone(),
                ));
                // `fg` — job-control resume of an editor suspended with Ctrl+Z.
                tools.register(crate::runtime::vi_builtin::FgBuiltin::new(
                    d.clone(),
                    Some(principal),
                ));
                // Replace the host process listing with Kaijutsu jobs.
                tools.register(crate::runtime::ps_builtin::PsBuiltin::new(true));
                tools.register(crate::runtime::kj_builtin::KjBuiltin::new(
                    d,
                    scm,
                    identity,
                    semantic_index,
                    block_source,
                    privileged, read_only,
                ));
                // The read-only interpreter replaces curl with an explicit
                // refusal after tool registration.
                tools.register(crate::runtime::curl_tool::curl_tool(
                    &egress_hosts,
                    classifier_host.as_deref(),
                ));
            };

        let kaish = if read_only {
            EmbeddedKaish::with_identity_read_only(
                name,
                dispatcher.block_store().clone(),
                dispatcher.kernel().clone(),
                inputs.cwd.clone(),
                identity,
                session_contexts,
                configure_tools,
            )?
        } else {
            EmbeddedKaish::with_identity(
                name,
                dispatcher.block_store().clone(),
                dispatcher.kernel().clone(),
                inputs.cwd.clone(),
                identity,
                session_contexts,
                inputs.external_exec,
                output,
                configure_tools,
            )?
        };

        kaish.export_env_vars(&inputs.exports).await?;
        // Validate the initial directory without changing its scope after seeding.
        if let Some(path) = &inputs.cwd {
            if !kaish.try_set_cwd(path.clone()).await {
                kaijutsu_telemetry::record_cwd_restore_failed();
                if matches!(cwd, ShellCwd::Captured(_)) {
                    anyhow::bail!("approved cwd '{}' no longer resolves to a directory; nothing was run", path.display());
                }
                anyhow::bail!("context cwd '{}' is unavailable; set a valid cwd before executing", path.display());
            }
        }

        Ok(kaish)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::synthesis::NoopBlockSource;
    use crate::kj::test_helpers::*;
    use kaish_kernel::ExecuteOptions;

    #[tokio::test]
    async fn contextual_shell_requires_registered_dispatcher() {
        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("unwired"), None, principal);
        let result = EmbeddedKaish::for_context(
            &d,
            "unwired",
            ShellIdentity {
                requester: principal, performer: principal, reviewer: None,
                context: ctx, session: SessionId::new(),
            },
            ShellPolicy::Agent, ShellCwd::Context,
            None,
            Arc::new(NoopBlockSource),
        ).await;
        let error = match result {
            Ok(_) => panic!("contextual construction must not silently omit kj and editor builtins"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("dispatcher"), "{error}");
    }

    #[tokio::test]
    async fn contextual_shell_reports_binding_read_failure() {
        let d = Arc::new(test_dispatcher().await);
        d.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("broken-binding"), None, principal);
        d.kernel().broker().set_db(d.kernel_db().clone()).await;
        {
            let mut db = d.kernel_db().lock();
            db.upsert_context_binding(ctx, &crate::mcp::ContextToolBinding::default()).unwrap();
            db.poison_context_binding_detail_table_for_test().unwrap();
        }
        let result = EmbeddedKaish::for_context(&d, "broken-binding", ShellIdentity {
            requester: principal, performer: principal, reviewer: None,
            context: ctx, session: SessionId::new(),
        }, ShellPolicy::Internal, ShellCwd::Context, None, Arc::new(NoopBlockSource)).await;
        let error = match result {
            Ok(_) => panic!("a binding read failure must refuse construction"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains(&ctx.to_string()), "{error}");
    }

    #[tokio::test]
    async fn contextual_shell_reports_cwd_read_failure() {
        let d = Arc::new(test_dispatcher().await);
        d.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("broken-cwd"), None, principal);
        d.kernel_db().lock().conn_for_ledger().execute_batch(
            "ALTER TABLE context_shell RENAME TO unavailable_context_shell"
        ).unwrap();
        let result = EmbeddedKaish::for_context(&d, "broken-cwd", ShellIdentity {
            requester: principal, performer: principal, reviewer: None,
            context: ctx, session: SessionId::new(),
        }, ShellPolicy::Internal, ShellCwd::Context, None, Arc::new(NoopBlockSource)).await;
        let error = match result {
            Ok(_) => panic!("a cwd read failure must refuse construction"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("context_shell"), "{error}");
    }

    #[tokio::test]
    async fn stored_and_captured_cwd_must_be_absolute() {
        let d = Arc::new(test_dispatcher().await);
        d.set_self_arc();
        let principal = PrincipalId::new();
        let context = register_context(&d, Some("relative-cwd"), None, principal);
        d.kernel_db().lock().upsert_context_shell(&crate::kernel_db::ContextShellRow {
            context_id: context, cwd: Some("relative/path".into()), updated_at: 0,
        }).unwrap();
        for cwd in [ShellCwd::Context, ShellCwd::Captured(Some("relative/path".into()))] {
            let result = EmbeddedKaish::for_context(&d, "relative-cwd", ShellIdentity {
                requester: principal, performer: principal, reviewer: None,
                context, session: SessionId::new(),
            }, ShellPolicy::Internal, cwd, None, Arc::new(NoopBlockSource)).await;
            let error = match result {
                Ok(_) => panic!("relative cwd must refuse construction"),
                Err(error) => error.to_string(),
            };
            assert!(error.contains("must be absolute"), "{error}");
        }
    }

    #[tokio::test]
    async fn contextual_shell_reports_unavailable_cwd() {
        let d = Arc::new(test_dispatcher().await);
        d.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("missing-cwd"), None, principal);
        let missing = "/unmounted/context-shell-missing-cwd";
        d.kernel_db().lock().upsert_context_shell(&crate::kernel_db::ContextShellRow {
            context_id: ctx, cwd: Some(missing.into()), updated_at: 0,
        }).unwrap();
        let result = EmbeddedKaish::for_context(&d, "missing-cwd", ShellIdentity {
            requester: principal, performer: principal, reviewer: None,
            context: ctx, session: SessionId::new(),
        }, ShellPolicy::Internal, ShellCwd::Context, None, Arc::new(NoopBlockSource)).await;
        let error = match result {
            Ok(_) => panic!("an unavailable persisted cwd must not run in a different directory"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains(missing), "{error}");
    }

    #[tokio::test]
    async fn captured_cwd_does_not_depend_on_newer_context_state() {
        let d = Arc::new(test_dispatcher().await);
        d.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("captured-cwd"), None, principal);
        d.kernel().mount("/approved", crate::vfs::backends::MemoryBackend::new()).await;
        d.kernel_db().lock().upsert_context_shell(&crate::kernel_db::ContextShellRow {
            context_id: ctx, cwd: Some("/missing/current-cwd".into()), updated_at: 0,
        }).unwrap();
        for pinned in [None, Some(std::path::PathBuf::from("/approved"))] {
            let kaish = EmbeddedKaish::for_context(&d, "captured-cwd", ShellIdentity {
                requester: principal, performer: principal, reviewer: None,
                context: ctx, session: SessionId::new(),
            }, ShellPolicy::Agent, ShellCwd::Captured(pinned.clone()), None, Arc::new(NoopBlockSource)).await.unwrap();
            let expected = pinned.unwrap_or_else(kaish_kernel::home_dir);
            assert_eq!(kaish.cwd().await, expected, "captured state wins, including an unset cwd");
        }
    }

    /// Wire a dispatcher whose kernel carries the FULL builtin MCP server set
    /// (block/file/shell/tool_search/…) and whose broker knows the dispatcher —
    /// the runtime shape a live context shell runs in. The bare `test_dispatcher`
    /// leaves the broker empty, so an unknown command's fall-through to the
    /// backend tool lookup never traverses the real registry there; this does.
    async fn dispatcher_with_full_broker() -> Arc<KjDispatcher> {
        let d = Arc::new(test_dispatcher().await);
        d.set_self_arc();
        let store = d.block_store().clone();
        let file_cache = d.kernel().file_cache().clone();
        d.kernel()
            .register_builtin_mcp_servers(store, file_cache, None, d.kernel_db().clone())
            .await
            .expect("register builtin mcp servers");
        d.kernel().broker().set_kj_dispatcher(&d).await;
        d
    }

    /// Grant `ctx` a broad, `all_instances` binding in the broker's in-memory
    /// map so the unknown-command fall-through's `list_visible_tools` actually
    /// walks the full registered server set — the real shape a broad context
    /// (coder/toolie) dispatches in. `exec` controls whether the shell may spawn
    /// host subprocesses (the `mount`-wedge axis); it is never implied by `*`.
    async fn grant_broad_binding(d: &Arc<KjDispatcher>, ctx: ContextId, exec: bool) {
        let mut binding = crate::mcp::ContextToolBinding {
            all_instances: true,
            all_facades: true,
            ..Default::default()
        };
        if exec {
            binding.grant(crate::mcp::Capability::Exec);
        }
        d.kernel().broker().set_binding(ctx, binding).await.unwrap();
    }

    struct IdentityProbe {
        id: crate::mcp::InstanceId,
        calls: Arc<parking_lot::Mutex<Vec<crate::mcp::CallContext>>>,
    }

    #[async_trait::async_trait]
    impl crate::mcp::McpServerLike for IdentityProbe {
        fn instance_id(&self) -> &crate::mcp::InstanceId { &self.id }
        async fn list_tools(&self, _: &crate::mcp::CallContext) -> crate::mcp::McpResult<Vec<crate::mcp::KernelTool>> {
            Ok(vec![crate::mcp::KernelTool { instance: self.id.clone(), name: "identity-probe".into(),
                description: None, input_schema: serde_json::json!({"type":"object","properties":{}}) }])
        }
        async fn call_tool(&self, _: crate::mcp::KernelCallParams, ctx: &crate::mcp::CallContext,
            _: tokio_util::sync::CancellationToken) -> crate::mcp::McpResult<crate::mcp::KernelToolResult> {
            self.calls.lock().push(ctx.clone());
            Ok(crate::mcp::KernelToolResult { is_error: false, content: vec![], structured: None })
        }
        fn notifications(&self) -> tokio::sync::broadcast::Receiver<crate::mcp::ServerNotification> {
            tokio::sync::broadcast::channel(1).1
        }
    }

    #[tokio::test]
    async fn mcp_dispatch_preserves_complete_invocation_identity() {
        for read_only in [false, true] {
            let d = dispatcher_with_full_broker().await;
            let requester = PrincipalId::new();
            let initial = register_context(&d, Some("probe-initial"), None, requester);
            let switched = register_context(&d, Some("probe-switched"), None, requester);
            for context in [initial, switched] { grant_broad_binding(&d, context, false).await; }
            let calls = Arc::new(parking_lot::Mutex::new(Vec::new()));
            d.kernel().broker().register_silently(Arc::new(IdentityProbe {
                id: crate::mcp::InstanceId("identity-probe".into()), calls: calls.clone(),
            }), crate::mcp::InstancePolicy::default()).await.unwrap();
            let identity = ShellIdentity { requester, performer: PrincipalId::new(), reviewer: Some(PrincipalId::new()),
                context: initial, session: SessionId::new() };
            let kaish = EmbeddedKaish::for_context(&d, "probe", identity,
                if read_only { ShellPolicy::ReadOnly } else { ShellPolicy::Agent },
                ShellCwd::Context, None, Arc::new(NoopBlockSource)).await.unwrap();
            kaish.set_context_id(switched);
            let result = kaish.execute_with_options("identity-probe", ExecuteOptions::default()).await.unwrap();
            if read_only {
                assert!(!result.ok() && result.err.contains("read-only"), "{result:?}");
                assert!(calls.lock().is_empty(), "read-only refusal happens before MCP dispatch");
                continue;
            }
            assert!(result.ok(), "{result:?}");
            let calls = calls.lock();
            assert_eq!(calls.len(), 1);
            let observed = &calls[0];
            assert_eq!(observed.principal_id, identity.requester);
            assert_eq!(observed.actor_id, identity.performer);
            assert_eq!(observed.reviewer_id, identity.reviewer);
            assert_eq!(observed.session_id, identity.session);
            assert_eq!(observed.context_id, switched);
        }
    }

    #[tokio::test]
    async fn mcp_block_authorship_preserves_shell_performer_after_context_switch() {
        let d = dispatcher_with_full_broker().await;
        let requester = PrincipalId::new();
        let performer = PrincipalId::new();
        let initial = register_context(&d, Some("identity-initial"), None, requester);
        let switched = register_context(&d, Some("identity-switched"), None, requester);
        for context in [initial, switched] {
            grant_broad_binding(&d, context, false).await;
            d.block_store().create_document(context, kaijutsu_types::DocKind::Conversation, None).unwrap();
        }
        let kaish = EmbeddedKaish::for_context(&d, "mcp-identity", ShellIdentity {
            requester, performer, reviewer: Some(PrincipalId::new()), context: initial, session: SessionId::new(),
        }, ShellPolicy::Agent, ShellCwd::Context, None, Arc::new(NoopBlockSource)).await.unwrap();
        kaish.set_context_id(switched);
        for command in [
            "block_create --role user --kind text --content performer-block",
            "svg_block --content '<svg xmlns=\"http://www.w3.org/2000/svg\"/>'",
            "task_create --content performer-task",
        ] {
            let result = kaish.execute_with_options(command, ExecuteOptions::default()).await.unwrap();
            assert!(result.ok(), "{command}: {result:?}");
        }
        let blocks = d.block_store().block_snapshots(switched).unwrap();
        assert_eq!(blocks.len(), 3);
        for block in blocks {
            assert_eq!(block.author(), performer, "MCP writes belong to the performer: {}", block.content);
        }
        assert!(d.block_store().block_snapshots(initial).unwrap().is_empty());
    }

    #[tokio::test]
    async fn editor_reads_preserve_performer_and_context_at_open() {
        for front_door in ["vi", "edit", "kj editor open"] {
            let d = dispatcher_with_full_broker().await;
            let requester = PrincipalId::new();
            let performer = PrincipalId::new();
            let initial = register_context(&d, Some("editor-initial"), None, requester);
            let switched = register_context(&d, Some("editor-switched"), None, requester);
            for context in [initial, switched] {
                grant_broad_binding(&d, context, false).await;
                d.block_store().create_document(context, kaijutsu_types::DocKind::Conversation, None).unwrap();
            }
            use crate::vfs::VfsOps;
            d.kernel().vfs().write_all(std::path::Path::new("/config/kernel/identity-editor.txt"), b"original").await.unwrap();
            let calls = Arc::new(parking_lot::Mutex::new(Vec::new()));
            d.kernel().broker().register_silently(Arc::new(IdentityProbe {
                id: crate::mcp::InstanceId("editor-identity-probe".into()), calls: calls.clone(),
            }), crate::mcp::InstancePolicy::default()).await.unwrap();
            let identity = ShellIdentity {
                requester, performer, reviewer: Some(PrincipalId::new()), context: initial, session: SessionId::new(),
            };
            let kaish = EmbeddedKaish::for_context(&d, "editor-identity", identity,
                ShellPolicy::Agent, ShellCwd::Context, None, Arc::new(NoopBlockSource)).await.unwrap();
            kaish.set_context_id(switched);
            let opened = kaish.execute_with_options(&format!("{front_door} /config/kernel/identity-editor.txt"), ExecuteOptions::default()).await.unwrap();
            assert!(opened.ok(), "{front_door}: {opened:?}");
            let data = kaish_kernel::interpreter::value_to_json(opened.data.as_ref().unwrap());
            let session = crate::editor::EditorSessionId::from_u64(data["session"].as_u64().unwrap());
            // Later shell switches do not retarget an existing editor's read.
            kaish.set_context_id(initial);
            let state = d.kernel().editor_keys(session, ":r !kj block create --role user --kind text --content editor-performer; identity-probe<CR>", requester).await.unwrap();
            let target = crate::editor::resolve_editor_target("/config/kernel/identity-editor.txt", d.kernel().file_cache()).await.unwrap();
            assert_eq!(d.block_store().get(target.context_id).unwrap().doc.principal_id(), requester,
                "the shell read keeps the opener identity, but its insertion belongs to the current input actor");
            let blocks = d.block_store().block_snapshots(switched).unwrap();
            let block = blocks.iter().find(|block| block.content == "editor-performer")
                .unwrap_or_else(|| panic!("{front_door}: read must author in context at open; {:?}", state.message));
            assert_eq!(block.author(), performer, "{front_door}: read must preserve performer");
            {
                let calls = calls.lock();
                assert_eq!(calls.len(), 1, "{front_door}: editor read must reach MCP");
                let observed = &calls[0];
                assert_eq!(observed.principal_id, identity.requester);
                assert_eq!(observed.actor_id, identity.performer);
                assert_eq!(observed.reviewer_id, identity.reviewer);
                assert_eq!(observed.session_id, identity.session);
                assert_eq!(observed.context_id, switched);
            }
            let initial_blocks = d.block_store().block_snapshots(initial).unwrap();
            assert!(initial_blocks.is_empty(), "{front_door}: unexpected initial-context blocks: {initial_blocks:?}");
            d.kernel().shutdown_runtime_worker().await.unwrap();
        }
    }

    #[tokio::test]
    async fn read_only_model_shell_refuses_mutating_tools_and_nested_editor_reads() {
        use crate::mcp::{CallContext, KernelCallParams, InstanceId};
        use crate::vfs::VfsOps;
        let d = dispatcher_with_full_broker().await;
        let principal = PrincipalId::new();
        let context = register_context(&d, Some("readonly-tools"), None, principal);
        grant_broad_binding(&d, context, false).await;
        d.block_store().create_document(context, kaijutsu_types::DocKind::Conversation, None).unwrap();
        let path = "/config/kernel/readonly-editor.txt";
        d.kernel().vfs().write_all(std::path::Path::new(path), b"original").await.unwrap();
        let opener = crate::editor::EditorOpener { principal, performer: principal, reviewer: None,
            context_id: context, session_id: SessionId::new() };
        let (editor, _) = d.kernel().editor_open_as(path, Some(opener)).await.unwrap();
        let call_context = CallContext::new(principal, context, SessionId::new(), d.kernel_id());
        let mut wrong = Vec::new();
        for command in [
            "kj block create --role user --kind text --content readonly-must-not-write".to_string(),
            "kj block $(echo create) --role user --kind text --content readonly-must-not-write".to_string(),
            "kj block create --confirm --role user --kind text --content readonly-must-not-write".to_string(),
            "kj synth all".to_string(),
            "kj synth rebuild".to_string(),
            "block_create --role user --kind text --content readonly-must-not-write".to_string(),
            format!("vi {path}"),
            format!("edit {path}"),
            format!("kj editor keys {} ':r !echo readonly-must-not-write<CR>'", editor.as_u64()),
            "curl -X POST http://127.0.0.1:9".to_string(),
        ] {
            let result = d.kernel().broker().call_tool(KernelCallParams {
                instance: InstanceId::new(crate::mcp::servers::shell::ShellServer::INSTANCE),
                tool: "shell".into(),
                arguments: serde_json::json!({"command": command, "foreground": true}),
            }, &call_context, tokio_util::sync::CancellationToken::new()).await.unwrap();
            let text = format!("{result:?}");
            if !result.is_error || !text.contains("read-only") {
                wrong.push(format!("{command}: {text}"));
            }
        }
        assert!(wrong.is_empty(), "mutating tools must refuse before dispatch:\n{}", wrong.join("\n"));
        assert!(!d.block_store().block_snapshots(context).unwrap().iter().any(|b| b.content == "readonly-must-not-write"));
        assert_eq!(d.kernel().editor_state(editor).unwrap().text, "original");
        assert_eq!(d.kernel().editor_list().len(), 1, "refused vi/edit must not allocate sessions");
        let mut failed_reads = Vec::new();
        for command in ["kj context list", "kj block list", "kj editor open --help", "cat /config/kernel/readonly-editor.txt",
            "kj help", "kj context create help", "kj synth status", "kj synth help", "kj synth --help"] {
            let result = d.kernel().broker().call_tool(KernelCallParams {
                instance: InstanceId::new(crate::mcp::servers::shell::ShellServer::INSTANCE), tool: "shell".into(),
                arguments: serde_json::json!({"command": command, "foreground": true}),
            }, &call_context, tokio_util::sync::CancellationToken::new()).await.unwrap();
            if result.is_error { failed_reads.push(format!("{command}: {result:?}")); }
        }
        assert!(failed_reads.is_empty(), "read-only inspection remains available: {}", failed_reads.join("\n"));
        d.kernel().shutdown_runtime_worker().await.unwrap();
    }

    async fn assert_model_shell_cannot_reach_compose_draft(command: &str) {
        for read_only in [true, false] {
            let d = dispatcher_with_full_broker().await;
            let requester = PrincipalId::new();
            let performer = PrincipalId::new();
            let ctx = register_context(&d, Some("draft-isolation"), None, requester);
            grant_broad_binding(&d, ctx, false).await;
            d.block_store()
                .create_document(ctx, kaijutsu_types::DocKind::Conversation, None)
                .unwrap();
            let drafts = [(requester, "requester unfinished draft"), (performer, "performer unfinished draft")];
            for (principal, text) in drafts {
                d.block_store().edit_draft(ctx, principal, 0, text, 0).unwrap();
            }

            let kaish = EmbeddedKaish::for_context(
                &d,
                "draft-isolation",
                ShellIdentity {
                    requester, performer, reviewer: Some(requester),
                    context: ctx, session: SessionId::new(),
                },
                if read_only { ShellPolicy::ReadOnly } else { ShellPolicy::Agent }, ShellCwd::Context,
                None,
                Arc::new(NoopBlockSource),
            ).await.expect("materialize model shell");

            let ready = kaish.execute_with_options("echo shell-ready", ExecuteOptions::default())
                .await.unwrap();
            assert!(ready.ok(), "model shell must otherwise work: {}", ready.err);
            assert_eq!(ready.text_out().trim(), "shell-ready");

            let draft_id = d.block_store().draft_block(ctx, requester).unwrap().unwrap().id;
            let command = command.replace("{draft}", &draft_id.to_key())
                .replace("{context}", &ctx.to_hex());
            let result = kaish.execute_with_options(&command, ExecuteOptions::default())
                .await.expect("valid command must return an execution result");
            assert!(!result.ok(), "read_only={read_only}: {command} must fail, got {result:?}");
            for (principal, text) in drafts {
                assert!(!result.text_out().contains(text), "read_only={read_only}: draft leaked");
                let draft = d.block_store().draft_block(ctx, principal).unwrap().unwrap();
                assert_eq!(draft.content, text, "read_only={read_only}: {command} changed a draft");
            }
        }
    }

    #[tokio::test]
    async fn model_shells_cannot_reach_drafts_through_docs() {
        for command in [
            "cat /v/docs/{context}/{draft}",
            "echo replacement > /v/docs/{context}/{draft}",
            "rm /v/docs/{context}/{draft}",
        ] {
            assert_model_shell_cannot_reach_compose_draft(command).await;
        }
    }

    #[tokio::test]
    async fn model_shells_cannot_read_compose_draft() {
        assert_model_shell_cannot_reach_compose_draft("cat /v/input").await;
    }

    #[tokio::test]
    async fn model_shells_cannot_write_compose_draft() {
        assert_model_shell_cannot_reach_compose_draft("echo replacement > /v/input").await;
    }

    #[tokio::test]
    async fn model_shells_cannot_clear_compose_draft() {
        assert_model_shell_cannot_reach_compose_draft("rm /v/input").await;
    }

    /// Unknown commands must fail promptly even with the full broker registry.
    #[tokio::test]
    async fn unknown_command_fails_fast_exec_denied_shell() {
        let d = dispatcher_with_full_broker().await;
        let principal = PrincipalId::new();
        // register_context grants a broad loadout but NOT Exec, so this
        // materializes a Deny shell; the broad binding makes the fall-through
        // walk every registered server.
        let ctx = register_context(&d, Some("deny"), None, principal);
        grant_broad_binding(&d, ctx, false).await;

        let kaish = EmbeddedKaish::for_context(
            &d,
            "unknown-cmd-deny",
            ShellIdentity {
                requester: principal, performer: principal, reviewer: None,
                context: ctx, session: SessionId::new(),
            },
            ShellPolicy::Agent, ShellCwd::Context,
            None,
            Arc::new(NoopBlockSource),
        )
            .await
            .expect("materialize context shell");

        let res = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            kaish.execute_with_options("mount", ExecuteOptions::default()),
        )
        .await
        .expect("unknown command must fail fast, not hang the shell timeout")
        .expect("exec returns");
        assert_eq!(
            res.code, 127,
            "unknown command should be command-not-found (127): {}",
            res.err
        );
    }

    /// Read-only shells deny host execution and resolve unknown names promptly.
    #[tokio::test]
    async fn unknown_command_fails_fast_read_only_shell() {
        let d = dispatcher_with_full_broker().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("toolie"), None, principal);
        grant_broad_binding(&d, ctx, false).await;

        let kaish = EmbeddedKaish::for_context(
            &d,
            "unknown-cmd-ro",
            ShellIdentity {
                requester: principal, performer: principal, reviewer: None,
                context: ctx, session: SessionId::new(),
            },
            ShellPolicy::ReadOnly, ShellCwd::Context,
            None,
            Arc::new(NoopBlockSource),
        )
            .await
            .expect("materialize read-only context shell");

        let res = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            kaish.execute_with_options("mount", ExecuteOptions::default()),
        )
        .await
        .expect("unknown command must fail fast in a read-only shell, not hang")
        .expect("exec returns");
        assert_eq!(
            res.code, 127,
            "unknown command should be command-not-found (127): {}",
            res.err
        );
    }

    /// Granted host execution resolves a real binary and rejects unknown names.
    /// `id` has bounded output, keeping this independent of the output spill cap.
    #[tokio::test]
    async fn unknown_command_fails_fast_exec_granted_shell() {
        let d = dispatcher_with_full_broker().await;
        // Real host root so the shell's default cwd ($HOME) resolves to a real
        // directory — the same shape as production's read-only "/" mount.
        d.kernel()
            .mount("/", crate::vfs::backends::LocalBackend::read_only("/"))
            .await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("allow"), None, principal);
        grant_broad_binding(&d, ctx, true).await;

        let kaish = EmbeddedKaish::for_context(
            &d,
            "unknown-cmd-allow",
            ShellIdentity {
                requester: principal, performer: principal, reviewer: None,
                context: ctx, session: SessionId::new(),
            },
            ShellPolicy::Agent, ShellCwd::Context,
            None,
            Arc::new(NoopBlockSource),
        )
            .await
            .expect("materialize context shell");

        // A real host binary resolves on PATH and spawns — must return promptly.
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            kaish.execute_with_options("id", ExecuteOptions::default()),
        )
        .await
        .expect("`id` must return promptly, not hang the shell timeout")
        .expect("exec returns");
        assert!(
            res.ok(),
            "`id` should run and exit 0 in an exec-granted shell: {}",
            res.err
        );

        // A name on neither PATH nor the registry falls through and must 127.
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            kaish.execute_with_options(
                "definitely_not_a_real_binary_kaijutsu_xyz",
                ExecuteOptions::default(),
            ),
        )
        .await
        .expect("unknown command must fail fast, not hang the shell timeout")
        .expect("exec returns");
        assert_eq!(
            res.code, 127,
            "unknown command should be command-not-found (127): {}",
            res.err
        );
    }

    #[tokio::test]
    async fn captured_environment_matches_materialized_scope_and_exec_policy() {
        let d = dispatcher_with_full_broker().await;
        d.kernel().mount("/", crate::vfs::backends::LocalBackend::read_only("/")).await;
        let cwd = tempfile::tempdir().unwrap();
        let principal = PrincipalId::new();
        let context = register_context(&d, Some("effective-env"), None, principal);
        grant_broad_binding(&d, context, true).await;
        d.kernel_db().lock().upsert_context_shell(&crate::kernel_db::ContextShellRow {
            context_id: context, cwd: Some(cwd.path().to_string_lossy().into_owned()), updated_at: 0,
        }).unwrap();
        for (read_only, overridden) in [(false, false), (true, false), (false, true), (true, true)] {
            if overridden {
                let db = d.kernel_db().lock();
                for (name, value) in [("HOME", "/configured-home"), ("PWD", "/configured-pwd"), ("PATH", "/configured-bin")] {
                    db.set_context_env(context, name, value).unwrap();
                }
            }
            let inputs = ContextShellInputs::load(d.kernel(), context, read_only, ShellCwd::Context).await.unwrap();
            let captured = inputs.environment();
            let shell = EmbeddedKaish::for_context(&d, "effective-env", ShellIdentity {
                requester: principal, performer: principal, reviewer: None, context, session: SessionId::new(),
            }, if read_only { ShellPolicy::ReadOnly } else { ShellPolicy::Agent },
                ShellCwd::Context, None, Arc::new(NoopBlockSource)).await.unwrap();
            assert_eq!(captured.get("PATH").map(String::as_str), if overridden { Some("/configured-bin") } else if read_only { None } else { d.kernel().host_path() });
            for name in ["HOME", "PWD", "PATH", "UNSET"] {
                let value = shell.get_var(name).await.map(|value| kaish_kernel::interpreter::value_to_string(&value));
                assert_eq!(value.as_ref(), captured.get(name), "{name}, read_only={read_only}");
                if let Some(value) = captured.get(name) {
                    let result = shell.execute_with_options(&format!("echo \"${name}\""), ExecuteOptions::default()).await.unwrap();
                    assert_eq!(result.code, 0, "{name}: {}", result.err);
                    assert_eq!(result.text_out(), format!("{value}\n"), "{name}, read_only={read_only}");
                }
            }
            assert_eq!(shell.cwd().await, cwd.path());
        }
    }

    /// Every invocation reads the context's durable environment.
    #[tokio::test]
    async fn materialized_shell_restores_durable_exports_and_vfs_cwd() {
        let d = Arc::new(test_dispatcher().await);
        d.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("here"), None, principal);
        use crate::vfs::VfsOps;
        d.kernel().mount("/scratch", crate::vfs::MemoryBackend::new()).await;
        d.kernel().vfs().mkdir(std::path::Path::new("/scratch/work"), 0o755).await.unwrap();
        let special = "it's a $HOME test\nwith a second line";
        {
            let db = d.kernel_db().lock();
            db.upsert_context_shell(&crate::kernel_db::ContextShellRow {
                context_id: ctx, cwd: Some("/scratch/work".into()), updated_at: 0,
            }).unwrap();
            db.set_context_env(ctx, "SPECIAL", special).unwrap();
            db.set_context_env(ctx, "NUMBER", "42").unwrap();
        }

        // L1: set a durable env var on the context.
        d.kernel_db()
            .lock()
            .set_context_env(ctx, "FOO", "bar")
            .unwrap();

        let kaish = EmbeddedKaish::for_context(
            &d,
            "test-ctx-shell",
            ShellIdentity {
                requester: principal, performer: principal, reviewer: None,
                context: ctx, session: SessionId::new(),
            },
            ShellPolicy::Agent, ShellCwd::Context,
            None,
            Arc::new(NoopBlockSource),
        )
            .await
            .expect("materialize context shell");
        assert_eq!(kaish.cwd().await, std::path::Path::new("/scratch/work"));
        let exported = kaish.exported_vars().await;
        assert!(exported.contains(&("SPECIAL".into(), special.into())));
        assert!(exported.contains(&("NUMBER".into(), "42".into())));

        let result = kaish
            .execute_with_options("echo $FOO", ExecuteOptions::default())
            .await
            .expect("run echo");
        assert_eq!(
            result.text_out().trim(),
            "bar",
            "durable context_env FOO should seed the materialized shell",
        );
    }

    /// `docs/egress.md`, "The rule": a context with an empty egress list
    /// reaches nothing, loopback included; after `--egress-allow 127.0.0.1`
    /// the same context's shell reaches a local mock server. Driven through
    /// a real context shell (`EmbeddedKaish::for_context`), not by calling
    /// `curl_tool` directly, so the wiring in `configure_tools` is what gets
    /// exercised. The shell reads the rows when it is built, so granting the
    /// host requires a fresh shell for the second command — matching "a
    /// running `curl` keeps the list it started with".
    ///
    /// Multi-thread runtime, deliberately: `kaish-tools-curl`'s ureq backend
    /// runs its blocking HTTP call directly on a current-thread runtime
    /// (`block_in_place_compat`), which would starve the `tokio::spawn`'d
    /// mock server below on the same executor and hang until `curl`'s own
    /// timeout fired.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn context_curl_reaches_loopback_only_after_the_context_grants_it() {
        use tokio::io::AsyncWriteExt;

        let d = Arc::new(test_dispatcher().await);
        d.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("curl-egress"), None, principal);
        let identity = ShellIdentity {
            requester: principal, performer: principal, reviewer: None,
            context: ctx, session: SessionId::new(),
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
        });
        let url = format!("http://127.0.0.1:{port}/");

        // Empty list: refused before any connection opens — the mock server
        // never sees a request for this call.
        let kaish = EmbeddedKaish::for_context(
            &d, "curl-egress-empty", identity, ShellPolicy::Agent, ShellCwd::Context,
            None, Arc::new(NoopBlockSource),
        ).await.unwrap();
        let refused = kaish
            .execute_with_options(&format!("curl {url}"), ExecuteOptions::default())
            .await
            .unwrap();
        assert!(!refused.ok(), "an empty egress list must refuse a loopback host: {}", refused.err);

        // Grant loopback, then rebuild the shell to pick up the change.
        d.kernel_db().lock().add_context_egress(ctx, "127.0.0.1").unwrap();
        let kaish = EmbeddedKaish::for_context(
            &d, "curl-egress-granted", identity, ShellPolicy::Agent, ShellCwd::Context,
            None, Arc::new(NoopBlockSource),
        ).await.unwrap();
        let allowed = kaish
            .execute_with_options(&format!("curl {url}"), ExecuteOptions::default())
            .await
            .unwrap();
        assert!(allowed.ok(), "granting 127.0.0.1 must let curl reach the mock server: {}", allowed.err);
        assert_eq!(allowed.text_out(), "ok");

        server.await.unwrap();
    }

    /// `docs/egress.md`, "The classifier host": an empty egress list still
    /// reaches the host of `[classifier] url` in `gate.toml` — the escape
    /// hatch the lfm2d pre-call hook needs from an otherwise-empty context.
    /// Driven through a real context shell, matching
    /// `context_curl_reaches_loopback_only_after_the_context_grants_it`
    /// above; no `add_context_egress` row here, since the whole point is
    /// that the classifier host opens without one.
    ///
    /// Multi-thread runtime: see that test's own note.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn context_curl_reaches_the_configured_classifier_host_with_an_empty_egress_list() {
        use tokio::io::AsyncWriteExt;

        let d = Arc::new(test_dispatcher().await);
        d.set_self_arc();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
        });
        let url = format!("http://127.0.0.1:{port}/");

        // Override the seeded default gate.toml with one whose classifier
        // points at the mock server — `MountTable::mount` replaces the
        // existing `/config/kernel` mount.
        let config_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            config_dir.path().join(crate::kj::gate_policy::GATE_CONFIG_FILE),
            format!("[classifier]\nurl = \"http://127.0.0.1:{port}\"\n"),
        )
        .unwrap();
        assert!(
            d.kernel()
                .mount(
                    kaijutsu_types::paths::CONFIG_ROOT,
                    crate::vfs::LocalBackend::new(config_dir.path()),
                )
                .await,
            "the test kernel must accept a /config/kernel remount"
        );

        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("curl-classifier"), None, principal);
        let identity = ShellIdentity {
            requester: principal, performer: principal, reviewer: None,
            context: ctx, session: SessionId::new(),
        };

        let kaish = EmbeddedKaish::for_context(
            &d, "curl-classifier", identity, ShellPolicy::Agent, ShellCwd::Context,
            None, Arc::new(NoopBlockSource),
        ).await.unwrap();
        let allowed = kaish
            .execute_with_options(&format!("curl {url}"), ExecuteOptions::default())
            .await
            .unwrap();
        assert!(
            allowed.ok(),
            "an empty egress list must still reach the configured classifier host: {}",
            allowed.err
        );
        assert_eq!(allowed.text_out(), "ok");

        server.await.unwrap();
    }

    /// Transient scope is isolated across invocations of a shared context.
    #[tokio::test]
    async fn materializations_are_independent_per_principal() {
        let d = Arc::new(test_dispatcher().await);
        d.set_self_arc();
        let alice = PrincipalId::new();
        let bob = PrincipalId::new();
        let ctx = register_context(&d, Some("shared"), None, alice);

        let ka = EmbeddedKaish::for_context(
            &d,
            "alice",
            ShellIdentity {
                requester: alice, performer: alice, reviewer: None,
                context: ctx, session: SessionId::new(),
            },
            ShellPolicy::Agent, ShellCwd::Context,
            None,
            Arc::new(NoopBlockSource),
        )
            .await
            .expect("materialize for alice");
        let kb = EmbeddedKaish::for_context(
            &d,
            "bob",
            ShellIdentity {
                requester: bob, performer: bob, reviewer: None,
                context: ctx, session: SessionId::new(),
            },
            ShellPolicy::Agent, ShellCwd::Context,
            None,
            Arc::new(NoopBlockSource),
        )
            .await
            .expect("materialize for bob");

        // A var set in one instance's scope must not bleed into the other —
        // transients are per-invocation, the durable channel is the DB.
        ka.execute_with_options("export ONLY_ALICE=1", ExecuteOptions::default())
            .await
            .expect("set in alice");
        let leaked = kb
            .execute_with_options("echo \"[$ONLY_ALICE]\"", ExecuteOptions::default())
            .await
            .expect("read in bob");
        assert_eq!(
            leaked.text_out().trim(),
            "[]",
            "transient scope must not leak between materialized instances",
        );
    }

    /// A plain (non-exported) variable stays inside its shell: neither another
    /// principal's shell nor a fresh materialization for the same principal
    /// sees it. Companion to the exported-variable check above.
    #[tokio::test]
    async fn unexported_variable_does_not_leak_to_another_shell() {
        let d = Arc::new(test_dispatcher().await);
        d.set_self_arc();
        let alice = PrincipalId::new();
        let bob = PrincipalId::new();
        let ctx = register_context(&d, Some("shared-plain"), None, alice);
        let mk = |name: &'static str, principal: PrincipalId| {
            let d = d.clone();
            async move {
                EmbeddedKaish::for_context(
                    &d,
                    name,
                    ShellIdentity {
                        requester: principal, performer: principal, reviewer: None,
                        context: ctx, session: SessionId::new(),
                    },
                    ShellPolicy::Agent, ShellCwd::Context,
                    None,
                    Arc::new(NoopBlockSource),
                )
                    .await
                    .expect("materialize shell")
            }
        };
        let ka = mk("alice", alice).await;
        let kb = mk("bob", bob).await;

        // Run through the durable write-back, as a shell call does.
        let before = crate::runtime::shell_state::snapshot_shell_state(&ka).await;
        let own = ka
            .execute_with_options("LOCAL_ONLY=1; echo \"[$LOCAL_ONLY]\"", ExecuteOptions::default())
            .await
            .expect("set in alice");
        assert_eq!(own.text_out().trim(), "[1]", "control: the setter sees its own variable");
        let after = crate::runtime::shell_state::snapshot_shell_state(&ka).await;
        crate::runtime::shell_state::persist_shell_state(d.kernel_db(), ctx, &before, &after)
            .expect("persist shell state");

        for (who, shell) in [("bob", &kb), ("a fresh alice", &mk("alice-2", alice).await)] {
            let leaked = shell
                .execute_with_options("echo \"[$LOCAL_ONLY]\"", ExecuteOptions::default())
                .await
                .expect("read in other shell");
            assert_eq!(
                leaked.text_out().trim(),
                "[]",
                "an unexported variable must not reach {who}",
            );
        }
    }

    /// Model synthesis sees context blocks; rc and hook sources are empty.
    #[tokio::test]
    async fn block_source_surfaces_real_blocks_where_noop_is_blind() {
        use crate::runtime::synthesis::NoopBlockSource;
        use kaijutsu_index::BlockSource as _;
        use kaijutsu_types::{BlockKind, ContentType, DocKind, Role, Status};

        let d = test_dispatcher().await;
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("synth"), None, principal);

        // Seed one block into the context's document.
        d.block_store()
            .create_document(ctx, DocKind::Conversation, None)
            .expect("create document");
        d.block_store()
            .insert_block(
                ctx,
                None,
                None,
                Role::Model,
                BlockKind::Text,
                "hello synthesis",
                Status::Done,
                ContentType::Plain,
            )
            .expect("insert block");

        // The real source sees the block; NoopBlockSource (rc/hook path) does not.
        let real = d
            .block_source()
            .block_snapshots(ctx)
            .expect("real snapshots");
        assert!(
            !real.is_empty(),
            "block_source must surface the context's blocks (the synthesis fix)",
        );
        let noop = NoopBlockSource
            .block_snapshots(ctx)
            .expect("noop snapshots");
        assert!(
            noop.is_empty(),
            "NoopBlockSource is the degraded path — it surfaces nothing",
        );

        // The index install round-trips (server wires it at bootstrap; None
        // when embeddings are off — the model shell then degrades gracefully).
        assert!(
            d.semantic_index().is_none(),
            "no index installed by default"
        );
        d.set_semantic_index(None);
        assert!(d.semantic_index().is_none());
    }
}
