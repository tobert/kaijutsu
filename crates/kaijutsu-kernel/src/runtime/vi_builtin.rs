//! `vi` / `edit` kaish builtin — open a kernel-owned editor session on a path.
//!
//! The canonical, ergonomic front door to the editor surface (`docs/vi.md`).
//! `vi /etc/rc/coder/create/S00-stance.kai` resolves the path to its owning CRDT
//! block and opens a session, returning the session handle + initial state. It
//! does **no editing logic of its own** — it is a thin alias onto the kernel's
//! shared `editor_open` primitive, the same primitive `kj editor open` and (when
//! it needs to open an editor) `kj rc edit` route through. One primitive, many
//! front doors.
//!
//! Opening signals the submitter's app windows to pop a renderer (the
//! `open_editor` peer signal, via `Kernel::editor_open_signaled`), threading the
//! caller's principal off the `ExecContext`. Best-effort: a headless `vi` (a
//! model, a test) with no app still opens a real session. See `docs/vi.md`.

use std::sync::Arc;

use async_trait::async_trait;

use kaish_kernel::interpreter::ExecResult;
use kaish_kernel::tools::{ParamSchema, ToolArgs, ToolCtx, ToolSchema};
use kaish_kernel::{ast::Value, Tool};

use kaijutsu_types::PrincipalId;

use crate::editor::EditorOpener;
use crate::kj::KjDispatcher;

/// kaish builtin that opens an editor session via `Kernel::editor_open`.
///
/// Registered once per user-facing name (`vi`, `edit`) so both resolve to the
/// same behaviour; `name` is the registry key this instance answers to.
///
/// The `opener` (caller principal + context) is captured at **construction**,
/// not via a `ToolCtx` downcast: the kaish interpreter hands builtins the kaish
/// `ExecContext`, which carries no kaijutsu principal/context, so a downcast to
/// our `ExecContext` always missed. `materialize_context_kaish` builds a fresh
/// instance per invocation with the live `(principal, context_id, session_id)`,
/// so each `ViBuiltin` already knows who opened it (mirrors `KjBuiltin`).
pub struct ViBuiltin {
    dispatcher: Arc<KjDispatcher>,
    name: &'static str,
    opener: Option<EditorOpener>,
}

impl ViBuiltin {
    pub fn new(dispatcher: Arc<KjDispatcher>, name: &'static str, opener: Option<EditorOpener>) -> Self {
        Self {
            dispatcher,
            name,
            opener,
        }
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
            "Open a vi editor session on a file or rc/config path (kernel-owned; \
             drive it with `kj editor keys/state/save/quit`).",
        )
        .param(
            ParamSchema::required("path", "string", "File or rc/config path to edit").positional(),
        )
        .example(
            "Edit a coder stance script",
            "vi /etc/rc/coder/create/S00-stance.kai",
        )
    }

    async fn execute(&self, args: ToolArgs, _ctx: &mut dyn ToolCtx) -> ExecResult {
        let path = match args.positional.first() {
            Some(Value::String(s)) => s.clone(),
            Some(other) => format!("{other:?}"),
            None => {
                return ExecResult::failure(
                    2,
                    format!("{}: missing path\nusage: {} <path>", self.name, self.name),
                );
            }
        };

        // The opener (captured at construction) is who `open_editor` fans the
        // renderer signal to, and whose context `:r !cmd` shells out in. A
        // headless instance (no opener) still opens a real session — it just
        // pops no window and can't `:r !cmd`.
        let blocks = self.dispatcher.block_store();
        match self
            .dispatcher
            .kernel()
            .editor_open_signaled(&path, blocks, self.opener)
            .await
        {
            Ok((id, st)) => {
                let session = id.as_u64();
                let mut result = ExecResult::success(format!(
                    "opened editor session {session} on {path} \
                     — drive it with `kj editor keys {session} …`",
                ));
                // The one shared editor-state shape (`EditorState::to_json`), so
                // a driver reads the session id + buffer the same way it would
                // from `kj editor open`.
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
    use crate::kj::test_helpers::{test_caller, test_dispatcher_crdt_rc};
    use crate::runtime::context_engine::{session_context_map, SessionContextMap};
    use crate::runtime::embedded_kaish::EmbeddedKaish;
    use kaijutsu_types::{ContextId, PrincipalId, SessionId};
    use kaish_kernel::ExecuteOptions;

    /// Unique rc path (parse_rc_path needs SXX-name form), off the seeded tree.
    const P: &str = "/etc/rc/vitest/create/S00-foo.kai";

    /// Build an `EmbeddedKaish` wired with the `vi` + `edit` builtins against the
    /// CRDT-rc dispatcher (so `/etc/rc` is the real ConfigCrdtFs mount).
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
                let opener = Some(EditorOpener {
                    principal: PrincipalId::system(),
                    context_id: ctx,
                    session_id: sid,
                });
                tools.register(ViBuiltin::new(dispatcher.clone(), "vi", opener));
                tools.register(ViBuiltin::new(dispatcher.clone(), "edit", opener));
            };

        EmbeddedKaish::with_identity(
            "test-vi",
            blocks,
            kernel,
            None,
            PrincipalId::system(),
            ctx,
            session_id,
            session_contexts,
            configure_tools,
        )
        .expect("EmbeddedKaish init")
    }

    /// `vi <rc path>` opens a real session on the owning block: the message names
    /// a session id and the structured `.data` carries the block's current text.
    /// This is the front-door equivalent of the `kj editor open` e2e.
    #[tokio::test]
    async fn vi_opens_a_session_on_the_owning_rc_block() {
        let d = Arc::new(test_dispatcher_crdt_rc().await);
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

    /// `edit` is the same behaviour under a second name.
    #[tokio::test]
    async fn edit_is_an_alias_for_vi() {
        let d = Arc::new(test_dispatcher_crdt_rc().await);
        let c = test_caller();
        let s = |v: &str| v.to_string();
        d.dispatch(&[s("rc"), s("add"), s(P), s("--content"), s("world")], &c)
            .await;

        let kaish = embedded_with_vi(d.clone(), ContextId::new()).await;
        let res = kaish
            .execute_with_options(&format!("edit {P}"), ExecuteOptions::default())
            .await
            .expect("kaish exec");

        assert!(res.ok(), "edit should succeed: {res:?}");
        assert!(res.text_out().contains("opened editor session"));
    }

    /// A path that resolves to nothing fails loud (no empty editor) — the
    /// resolver's fail-loud contract surfaced through the front door.
    #[tokio::test]
    async fn vi_on_a_missing_config_path_fails_loud() {
        let d = Arc::new(test_dispatcher_crdt_rc().await);
        let kaish = embedded_with_vi(d.clone(), ContextId::new()).await;
        let missing = "/etc/rc/vitest/create/S99-nope.kai";
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
