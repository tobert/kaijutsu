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
use crate::kj::lifecycle::RcAuthority;
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
    /// Filesystem writes, host execution, and shared job mutation are refused.
    ReadOnly,
    /// Only lifecycle orchestration can supply the authority to widen a loadout.
    Rc(RcAuthority),
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
        semantic_index: Option<Arc<kaijutsu_index::SemanticIndex>>,
        block_source: Arc<dyn kaijutsu_index::BlockSource>,
    ) -> Result<Self> {
        let ShellIdentity {
            requester: principal, performer: actor, reviewer,
            context: context_id, session: session_id,
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
        let configure_tools =
            move |scm: SessionContextMap,
                  sid: SessionId,
                  tools: &mut kaish_kernel::ToolRegistry| {
                let d = kj_dispatcher;
                // ToolCtx carries no Kaijutsu identity. Capture the opener
                // for editor commands and later foregrounding.
                let opener = Some(crate::editor::EditorOpener {
                    principal,
                    context_id,
                    session_id: sid,
                });
                // Both editor names share Kernel::editor_open.
                tools.register(crate::runtime::vi_builtin::ViBuiltin::new(
                    d.clone(),
                    "vi",
                    opener,
                ));
                tools.register(crate::runtime::vi_builtin::ViBuiltin::new(
                    d.clone(),
                    "edit",
                    opener,
                ));
                // `fg` — job-control resume of an editor suspended with Ctrl+Z.
                tools.register(crate::runtime::vi_builtin::FgBuiltin::new(
                    d.clone(),
                    Some(principal),
                ));
                // Replace the host process listing with Kaijutsu jobs.
                tools.register(crate::runtime::ps_builtin::PsBuiltin::new(true));
                tools.register(crate::runtime::kj_builtin::KjBuiltin::new_as(
                    d,
                    scm,
                    principal,
                    actor,
                    reviewer,
                    sid,
                    semantic_index,
                    block_source,
                    privileged,
                ));
                // Network access to the configured inference endpoint is
                // independent of filesystem and host-execution policy.
                tools.register(crate::runtime::curl_tool::curl_tool());
            };

        let kaish = if read_only {
            EmbeddedKaish::with_identity_read_only(
                name,
                dispatcher.block_store().clone(),
                dispatcher.kernel().clone(),
                None,
                principal,
                context_id,
                session_id,
                session_contexts,
                configure_tools,
            )?
        } else {
            // Host subprocess policy from the context's loadout: the `exec`
            // authority (deny-by-default — a context with no binding, or a
            // binding without the grant, gets no external commands). PATH is
            // the kernel's startup capture; kaish never reads OS env itself.
            let external_exec = if dispatcher
                .kernel()
                .broker()
                .binding(&context_id)
                .await
                .is_some_and(|b| b.allows(&crate::mcp::Capability::Exec))
            {
                crate::runtime::embedded_kaish::ExternalExec::Allow {
                    path: dispatcher.kernel().host_path().map(str::to_string),
                }
            } else {
                crate::runtime::embedded_kaish::ExternalExec::Deny
            };
            EmbeddedKaish::with_identity(
                name,
                dispatcher.block_store().clone(),
                dispatcher.kernel().clone(),
                None,
                principal,
                context_id,
                session_id,
                session_contexts,
                external_exec,
                output,
                configure_tools,
            )?
        };

        // Seed the env half of the context's durable state.
        kaish
            .apply_context_config(dispatcher.kernel_db(), context_id)
            .await?;

        // Restore the persisted cwd, validated against the shell's backend (the
        // VFS namespace `cd` uses — a host-FS check would wrongly reject
        // VFS-only cwds like /scratch or /v/docs). A persisted cwd that no
        // longer resolves is surfaced, not silently dropped.
        if let Err(dead) = kaish
            .restore_cwd_from_db(dispatcher.kernel_db(), context_id)
            .await
        {
            tracing::warn!(
                context = %context_id.to_hex(),
                cwd = %dead.display(),
                "persisted context cwd no longer resolves in backend; using default landing dir",
            );
            kaijutsu_telemetry::record_cwd_restore_failed();
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
            ShellPolicy::Agent,
            None,
            Arc::new(NoopBlockSource),
        ).await;
        let error = match result {
            Ok(_) => panic!("contextual construction must not silently omit kj and editor builtins"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("dispatcher"), "{error}");
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
                if read_only { ShellPolicy::ReadOnly } else { ShellPolicy::Agent },
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
            ShellPolicy::Agent,
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
            ShellPolicy::ReadOnly,
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
            ShellPolicy::Agent,
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

    /// Every invocation reads the context's durable environment.
    #[tokio::test]
    async fn materialized_shell_seeds_durable_env() {
        let d = Arc::new(test_dispatcher().await);
        d.set_self_arc();
        let principal = PrincipalId::new();
        let ctx = register_context(&d, Some("here"), None, principal);

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
            ShellPolicy::Agent,
            None,
            Arc::new(NoopBlockSource),
        )
            .await
            .expect("materialize context shell");

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
            ShellPolicy::Agent,
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
            ShellPolicy::Agent,
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
