//! `vi` opens kernel-owned editor sessions through the same entry
//! point as `kj vi open`. The opener retains the invocation identity and
//! context at open; editor signals target the requester. See `docs/vi.md`.

use std::sync::Arc;

use async_trait::async_trait;

use kaish_kernel::interpreter::ExecResult;
use kaish_kernel::tools::{ParamSchema, ToolArgs, ToolCtx, ToolSchema};
use kaish_kernel::{ast::Value, Tool};

use kaijutsu_types::PrincipalId;

use crate::editor::EditorOpener;
use crate::kj::KjDispatcher;
use super::context_shell::ShellIdentity;
use super::context_engine::{SessionContextExt, SessionContextMap};

/// kaish builtin that opens an editor session via `Kernel::editor_open`.
///
/// Registered under the name `vi`; `name` is the registry key this instance
/// answers to.
///
/// Requester, performer, reviewer, and session remain fixed for the invocation.
/// Resolve its current context when opening, then retain that complete opener
/// on the editor session for later shell reads.
pub struct ViBuiltin {
    dispatcher: Arc<KjDispatcher>,
    name: &'static str,
    identity: ShellIdentity,
    session_contexts: SessionContextMap,
}

impl ViBuiltin {
    pub fn new(dispatcher: Arc<KjDispatcher>, name: &'static str,
        identity: ShellIdentity, session_contexts: SessionContextMap) -> Self {
        Self { dispatcher, name, identity, session_contexts }
    }
}

#[async_trait]
impl Tool for ViBuiltin {
    fn name(&self) -> &str {
        self.name
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            self.name,
            "Open a vi editor session over one or more files (kernel-owned; \
             drive it with `kj vi keys/state/save/quit`). Several paths make an \
             argument list: `:n` and `:N` move through it. A path already open in \
             one of your sessions joins that session.",
        )
        .param(
            ParamSchema::required("path", "string", "Files to edit; quote a path with spaces")
                .positional(),
        )
        .example(
            "Edit a coder stance script",
            "vi /config/rc/coder/create/S00-stance.kai",
        )
        .example("Edit every Rust file here, one after another", "vi *.rs")
    }

    async fn execute(&self, args: ToolArgs, _ctx: &mut dyn ToolCtx) -> ExecResult {
        // kaish expands globs before the builtin runs, so `vi *.rs` arrives
        // as one word per file. A path with a space must be quoted to stay
        // one word.
        let paths: Vec<String> = args
            .positional
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => format!("{other:?}"),
            })
            .collect();
        if paths.is_empty() {
            return ExecResult::failure(
                2,
                format!("{}: missing path\nusage: {} <path>...", self.name, self.name),
            );
        }

        let Some(context_id) = self.session_contexts.current(&self.identity.session) else {
            return ExecResult::failure(1, format!("{}: no active context joined", self.name));
        };
        let opener = EditorOpener {
            principal: self.identity.requester, performer: self.identity.performer,
            reviewer: self.identity.reviewer, context_id, session_id: self.identity.session,
        };
        match self
            .dispatcher
            .kernel()
            .editor_open_signaled(&paths, Some(opener))
            .await
        {
            Ok((id, st)) => {
                let session = id.as_u64();
                let mut result = ExecResult::success(format!(
                    "opened editor session {session} on {}{} \
                     — drive it with `kj vi keys {session} …`",
                    st.path,
                    kaijutsu_types::editor::arg_position_label(st.arg_index, st.arg_count)
                        .map(|p| format!(" {p}"))
                        .unwrap_or_default(),
                ));
                // The one shared editor-state shape (`EditorState::to_json`), so
                // a driver reads the session id + buffer the same way it would
                // from `kj vi open`.
                result.data = Some(kaish_kernel::interpreter::json_to_value(st.to_json(id)));
                result
            }
            Err(e) => ExecResult::failure(1, format!("{}: {e}", self.name)),
        }
    }
}

/// kaish builtin `fg` — re-foreground the editor suspended with Ctrl+Z (the Unix
/// job-control metaphor: work in vi, Ctrl+Z to the shell, `fg` to come back).
/// Re-fires the `open_editor` signal for the caller's most-recent session, so the
/// app pops back to the editor via the same path a fresh `vi` uses.
pub struct FgBuiltin {
    dispatcher: Arc<KjDispatcher>,
    /// The principal running `fg` (captured at construction, like [`ViBuiltin`]'s
    /// opener — the kaish `ToolCtx` carries no kaijutsu principal). `resume_editor`
    /// prefers this principal's most-recent session. `None` for a headless build.
    caller: Option<PrincipalId>,
}

impl FgBuiltin {
    pub fn new(dispatcher: Arc<KjDispatcher>, caller: Option<PrincipalId>) -> Self {
        Self { dispatcher, caller }
    }
}

#[async_trait]
impl Tool for FgBuiltin {
    fn name(&self) -> &str {
        "fg"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "fg",
            "Resume the editor you suspended with Ctrl+Z (job-control foreground).",
        )
        .example("Return to vi after peeking at the shell", "fg")
    }

    async fn execute(&self, _args: ToolArgs, _ctx: &mut dyn ToolCtx) -> ExecResult {
        // The caller (captured at construction) is whose suspended editor we
        // re-foreground; the signal fans to their app windows.
        match self.dispatcher.kernel().resume_editor(self.caller).await {
            Ok((id, st)) => {
                let session = id.as_u64();
                let mut result = ExecResult::success(format!("resuming editor session {session}"));
                result.data = Some(kaish_kernel::interpreter::json_to_value(st.to_json(id)));
                result
            }
            Err(e) => ExecResult::failure(1, e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kj::test_helpers::{test_caller, test_dispatcher_rc};
    use crate::runtime::context_engine::{session_context_map, SessionContextMap};
    use crate::runtime::embedded_kaish::EmbeddedKaish;
    use kaijutsu_types::{ContextId, PrincipalId, SessionId};
    use kaish_kernel::ExecuteOptions;

    /// Unique rc path (parse_rc_path needs SXX-name form), off the seeded tree.
    const P: &str = "/config/rc/vitest/create/S00-foo.kai";

    /// Build an `EmbeddedKaish` wired with the `vi` builtin against the
    /// rc dispatcher (so `/config/rc` is the real seeded host-directory mount).
    async fn embedded_with_vi(dispatcher: Arc<KjDispatcher>, ctx: ContextId) -> EmbeddedKaish {
        let blocks = dispatcher.block_store().clone();
        let kernel = dispatcher.kernel().clone();
        let session_id = SessionId::new();
        let session_contexts = session_context_map();
        session_contexts.insert(session_id, ctx);

        let configure_tools =
            move |_scm: SessionContextMap,
                  sid: SessionId,
                  tools: &mut kaish_kernel::ToolRegistry| {
                let identity = ShellIdentity { requester: PrincipalId::system(), performer: PrincipalId::system(),
                    reviewer: None, context: ctx, session: sid };
                tools.register(ViBuiltin::new(dispatcher.clone(), "vi", identity, _scm));
            };

        EmbeddedKaish::with_identity(
            "test-vi",
            blocks,
            kernel,
            None,
            crate::runtime::context_shell::ShellIdentity { requester: PrincipalId::system(), performer: PrincipalId::system(), reviewer: None, context: ctx, session: session_id },
            session_contexts,
            crate::runtime::embedded_kaish::ExternalExec::Deny,
            crate::runtime::embedded_kaish::OutputProfile::Agent,
            configure_tools,
        )
        .expect("EmbeddedKaish init")
    }

    /// `vi <rc path>` opens a real session on the owning block: the message names
    /// a session id and the structured `.data` carries the block's current text.
    /// This is the front-door equivalent of the `kj vi open` e2e.
    #[tokio::test]
    async fn vi_opens_a_session_on_the_owning_rc_block() {
        let d = Arc::new(test_dispatcher_rc().await);
        let c = test_caller();
        let s = |v: &str| v.to_string();

        // Seed an rc script through the same VFS-direct path `kj rc` uses.
        d.dispatch(&[s("rc"), s("add"), s(P), s("--content"), s("hello")], &c)
            .await;

        let kaish = embedded_with_vi(d.clone(), ContextId::new()).await;
        let res = kaish
            .execute_with_options(&format!("vi {P}"), ExecuteOptions::default())
            .await
            .expect("kaish exec");

        assert!(res.ok(), "vi should succeed: {res:?}");
        let out = res.text_out();
        assert!(
            out.contains("opened editor session"),
            "vi should report a session handle: {out}"
        );
        let data = res.data.as_ref().expect("vi emits structured data");
        let json = kaish_kernel::interpreter::value_to_json(data);
        assert_eq!(json["text"], "hello", "session must bind the owning block");
        assert!(
            json["session"].as_u64().is_some(),
            "data carries a numeric session id: {json}"
        );
    }

    /// `vi a b` opens one session over both files: the first is current and
    /// the list holds two. An unquoted `vi my notes.md` is two files the same
    /// way, so the path that does not exist fails when it is visited.
    #[tokio::test]
    async fn vi_with_two_paths_opens_one_session_over_the_list() {
        let d = Arc::new(test_dispatcher_rc().await);
        let c = test_caller();
        let s = |v: &str| v.to_string();
        let second = "/config/rc/vitest/create/S01-bar.kai";
        d.dispatch(&[s("rc"), s("add"), s(P), s("--content"), s("hello")], &c).await;
        d.dispatch(&[s("rc"), s("add"), s(second), s("--content"), s("world")], &c).await;

        let kaish = embedded_with_vi(d.clone(), ContextId::new()).await;
        let res = kaish
            .execute_with_options(&format!("vi {P} {second}"), ExecuteOptions::default())
            .await
            .expect("kaish exec");

        assert!(res.ok(), "two paths open one session: {res:?}");
        assert!(res.text_out().contains("(1 of 2)"), "{}", res.text_out());
        let json = kaish_kernel::interpreter::value_to_json(res.data.as_ref().expect("data"));
        assert_eq!(json["text"], "hello");
        assert_eq!(json["path"], P);
        assert_eq!((json["arg_index"].as_u64(), json["arg_count"].as_u64()), (Some(0), Some(2)));
    }

    /// A quoted path with a space is one word, so it names one file. It
    /// fails here only because the file is not there.
    #[tokio::test]
    async fn vi_with_a_quoted_path_with_a_space_is_one_word() {
        let d = Arc::new(test_dispatcher_rc().await);
        let kaish = embedded_with_vi(d.clone(), ContextId::new()).await;
        let res = kaish
            .execute_with_options("vi \"/config/rc/vitest/create/S99 nope.kai\"", ExecuteOptions::default())
            .await
            .expect("kaish exec");

        assert!(!res.ok());
        assert!(res.err.contains("S99 nope.kai"), "one quoted word is one path: {}", res.err);
    }

    /// A path that resolves to nothing fails loud (no empty editor) — the
    /// resolver's fail-loud contract surfaced through the front door.
    #[tokio::test]
    async fn vi_on_a_missing_config_path_fails_loud() {
        let d = Arc::new(test_dispatcher_rc().await);
        let kaish = embedded_with_vi(d.clone(), ContextId::new()).await;
        let missing = "/config/rc/vitest/create/S99-nope.kai";
        let res = kaish
            .execute_with_options(&format!("vi {missing}"), ExecuteOptions::default())
            .await
            .expect("kaish exec");
        assert!(
            !res.ok(),
            "vi on a missing path must not silently open empty"
        );
    }
}
