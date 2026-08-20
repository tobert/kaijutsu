//! `kj editor` — drive the kernel-owned editor sessions over the kj surface.
//!
//! The programmatic face of the in-app editor (`docs/vi.md`): `open` resolves a
//! path to its owning kernel block and starts a session; `keys` feeds vim input
//! and mirrors the edits onto the block; `state` reads the buffer; `save`/`quit`
//! are `ZZ`/`ZQ`. The Bevy app renders these same kernel sessions, and a model
//! drives them through here — one surface, many hands.

use clap::{Parser, Subcommand};

use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};
use crate::editor::{EditorSessionId, EditorState};
use crate::mcp::Capability;

#[derive(Parser, Debug)]
#[command(
    name = "editor",
    about = "Drive kernel-owned vi editor sessions (open/keys/state/save/quit/list)",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct EditorArgs {
    #[command(subcommand)]
    command: EditorCommand,
}

#[derive(Subcommand, Debug)]
enum EditorCommand {
    /// Open an editor on a path, binding to the kernel block that owns it.
    Open {
        /// File or rc/config path to edit (e.g. /etc/rc/coder/create/S00.kai).
        path: String,
    },
    /// Feed vim keys to a session (e.g. "iX<Esc>", "dw", "<C-w>"). A batch
    /// that submits a write (`:w`, `:w!`, `:wq`, `:x`, `ZZ`, …) needs the
    /// `editor` capability; plain edits and navigation don't.
    Keys {
        /// Session handle from `kj editor open`.
        session: u64,
        /// Key sequence in vim notation.
        keys: String,
    },
    /// Print a session's current buffer/cursor/mode/dirty state.
    State {
        /// Session handle.
        session: u64,
    },
    /// Checkpoint the buffer as saved (`ZZ`); for a file, also flush to disk.
    /// Needs the `editor` capability.
    Save {
        /// Session handle.
        session: u64,
    },
    /// Roll the block back to the last checkpoint and close the session (`ZQ`).
    Quit {
        /// Session handle.
        session: u64,
    },
    /// List open editor sessions (session, path, dirty, mode, opener).
    List,
}

/// Structured `.data` for one session's state — an object (inspect-style), so
/// `kj editor state --json` yields a single record a driver can read. The shape
/// lives on [`EditorState`] so every editor front door emits the same record.
fn state_json(id: EditorSessionId, st: &EditorState) -> serde_json::Value {
    st.to_json(id)
}

/// The capability `kj editor save` gates on. A write reached through
/// `kj editor keys` (`:w`, `ZZ`) is refused at the flush inside the kernel,
/// not by re-parsing keystrokes here. See [`Capability::Editor`] for why this
/// is a dedicated authority rather than a reuse of `Tool{builtin.file, edit}`.
fn write_cap() -> Capability {
    Capability::Editor
}

/// The `:`-line verbs that report a write (`KeysUpdate.saved`,
/// `crates/kaijutsu-kernel/src/editor.rs`): `parse_ex_command`
/// (`crates/kaijutsu-editor/src/lib.rs`) matches these exactly, after
/// stripping one adjacent trailing `!`. No abbreviations, no ranges — the
/// dialect is this short list, verbatim.

impl KjDispatcher {
    pub(crate) async fn dispatch_editor(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<EditorArgs>();
        }
        let parsed = match EditorArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(
                        e.to_string(),
                        kaijutsu_types::ContentType::Plain,
                    );
                }
                return KjResult::Err(format!("kj editor: {e}"));
            }
        };

        let kernel = self.kernel();
        // Record the opener (principal + context) so `fg` and `:r !cmd` work;
        // a caller with no joined context degrades to a headless-style open.
        let opener = caller.context_id.map(|context_id| crate::editor::EditorOpener {
            principal: caller.principal_id,
            context_id,
            session_id: caller.session_id,
        });
        match parsed.command {
            EditorCommand::Open { path } => match kernel
                .editor_open_signaled(&path, opener)
                .await
            {
                Ok((id, st)) => KjResult::ok_with_data(
                    format!("opened editor session {} on {path}", id.as_u64()),
                    state_json(id, &st),
                ),
                Err(e) => KjResult::Err(format!("kj editor open: {e}")),
            },
            EditorCommand::Keys { session, keys } => {
                let id = EditorSessionId::from_u64(session);
                // A `:w` typed and left pending in an earlier call rides on
                // the session's current command_line; fold it in before
                // guessing whether THIS batch is the one that submits it.
                match kernel.editor_keys(id, &keys).await {
                    Ok(st) => {
                        // A dialect-level failure (bad `:cmd`, dirty-`:q` refusal,
                        // failed `:r`) rides the status line, not the error path —
                        // surface it in the human line so a driver can't miss it.
                        let mut line = format!(
                            "session {session}: {} mode, {} chars",
                            mode_label(&st),
                            st.text.chars().count()
                        );
                        if let Some(msg) = &st.message {
                            line.push_str(&format!(" — {msg}"));
                        }
                        KjResult::ok_with_data(line, state_json(id, &st))
                    }
                    Err(e) => KjResult::Err(format!("kj editor keys: {e}")),
                }
            }
            EditorCommand::State { session } => {
                let id = EditorSessionId::from_u64(session);
                match kernel.editor_state(id) {
                    Ok(st) => KjResult::ok_with_data(
                        format!(
                            "session {session}: {} mode{}",
                            mode_label(&st),
                            if st.dirty { ", modified" } else { "" }
                        ),
                        state_json(id, &st),
                    ),
                    Err(e) => KjResult::Err(format!("kj editor state: {e}")),
                }
            }
            EditorCommand::Save { session } => {
                if let Err(denied) = self.require_cap(caller, write_cap(), "editor save") {
                    return denied;
                }
                let id = EditorSessionId::from_u64(session);
                match kernel.editor_save(id).await {
                    Ok(st) => {
                        // A flush failure (E212) rides `st.message`, not the
                        // error path — surface it so a driver can't miss it,
                        // same as `Keys` below.
                        let mut line = format!("session {session}: saved");
                        if let Some(msg) = &st.message {
                            line.push_str(&format!(" — {msg}"));
                        }
                        KjResult::ok_with_data(line, state_json(id, &st))
                    }
                    Err(e) => KjResult::Err(format!("kj editor save: {e}")),
                }
            }
            EditorCommand::Quit { session } => {
                let id = EditorSessionId::from_u64(session);
                match kernel.editor_quit(id) {
                    Ok(()) => KjResult::ok(format!(
                        "session {session}: closed (rolled back to checkpoint)"
                    )),
                    Err(e) => KjResult::Err(format!("kj editor quit: {e}")),
                }
            }
            EditorCommand::List => {
                let sessions = kernel.editor_list();
                let line = if sessions.is_empty() {
                    "no open editor sessions".to_string()
                } else {
                    sessions
                        .iter()
                        .map(|s| {
                            let modified = if s.dirty { " [modified]" } else { "" };
                            let opener = s
                                .opener
                                .as_deref()
                                .map(|o| format!(" (opener {o})"))
                                .unwrap_or_default();
                            format!(
                                "session {}: {}{modified} {}{opener}",
                                s.session,
                                s.path,
                                mode_label_of(s.mode.as_deref()),
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                let data = serde_json::to_value(&sessions)
                    .expect("EditorSessionInfo serializes");
                KjResult::ok_with_data(line, data)
            }
        }
    }
}

/// Human label for the vim mode banner (`None` == normal).
fn mode_label(st: &EditorState) -> &str {
    mode_label_of(st.mode.as_deref())
}

/// Shared mode-word formatting for anything carrying an `Option<String>` mode
/// (an [`EditorState`] or a listed [`crate::editor::EditorSessionInfo`]).
fn mode_label_of(mode: Option<&str>) -> &str {
    mode.map(str::trim)
        .map(|s| s.trim_matches('-').trim())
        .unwrap_or("NORMAL")
}

#[cfg(test)]
mod tests {
    use crate::kj::test_helpers::*;
    use crate::kj::{KjCaller, KjDispatcher, KjResult};

    /// Unique rc path (parse_rc_path needs SXX-name form), avoiding the seeded tree.
    const P: &str = "/etc/rc/editortest/create/S00-foo.kai";

    fn session_of(r: &KjResult) -> u64 {
        match r {
            KjResult::Ok { data: Some(d), .. } => d
                .get("session")
                .and_then(|v| v.as_u64())
                .expect("session id in data"),
            other => panic!("expected ok-with-data, got {other:?}"),
        }
    }

    async fn read_rc(d: &KjDispatcher, path: &str) -> Option<String> {
        use crate::vfs::VfsOps as _;
        let bytes = d
            .kernel()
            .vfs()
            .read_all(std::path::Path::new(path))
            .await
            .ok()?;
        String::from_utf8(bytes).ok()
    }

    /// The headline e2e for the kj surface: `open` → `keys` mutates the *actual*
    /// rc document (read back through the VFS, proving editor → block →
    /// ConfigDocFs), and `quit` rolls it back to the opened content.
    #[tokio::test]
    async fn kj_editor_edits_the_rc_doc_and_quit_rolls_back() {
        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        d.dispatch(&[s("rc"), s("add"), s(P), s("--content"), s("hello")], &c)
            .await;

        let opened = d.dispatch(&[s("editor"), s("open"), s(P)], &c).await;
        let id = session_of(&opened);

        // Type "X" at the start; the edit must reach the rc doc on disk-of-record.
        d.dispatch(&[s("editor"), s("keys"), id.to_string(), s("iX<Esc>")], &c)
            .await;
        assert_eq!(
            read_rc(&d, P).await.as_deref(),
            Some("Xhello"),
            "kj editor keys must mutate the owning rc doc"
        );

        // State reports the live buffer + dirty.
        let st = d
            .dispatch(&[s("editor"), s("state"), id.to_string()], &c)
            .await;
        match st {
            KjResult::Ok { data: Some(dd), .. } => {
                assert_eq!(dd["text"], "Xhello");
                assert_eq!(dd["dirty"], true);
            }
            other => panic!("expected state data, got {other:?}"),
        }

        // ZQ rolls the rc doc back to what we opened.
        d.dispatch(&[s("editor"), s("quit"), id.to_string()], &c)
            .await;
        assert_eq!(
            read_rc(&d, P).await.as_deref(),
            Some("hello"),
            "kj editor quit must roll the rc doc back to the checkpoint"
        );
    }

    /// `kj editor list` is the census: an open session, however it was
    /// opened, must show up with its session id and path.
    #[tokio::test]
    async fn kj_editor_list_reports_the_open_session() {
        let d = test_dispatcher_rc().await;
        let c = test_caller();
        let s = |v: &str| v.to_string();

        d.dispatch(&[s("rc"), s("add"), s(P), s("--content"), s("hello")], &c)
            .await;
        let opened = d.dispatch(&[s("editor"), s("open"), s(P)], &c).await;
        let id = session_of(&opened);

        let listed = d.dispatch(&[s("editor"), s("list")], &c).await;
        match listed {
            KjResult::Ok { data: Some(dd), .. } => {
                let arr = dd.as_array().expect("list data is a JSON array");
                assert_eq!(arr.len(), 1, "one open session");
                assert_eq!(arr[0]["path"], P);
                assert_eq!(arr[0]["session"], id);
            }
            other => panic!("expected ok-with-data, got {other:?}"),
        }
    }

    // ── capability gate ───────────────────────────────────────────────────

    /// A second rc path for the capability tests, seeded by a privileged
    /// caller (rc-write is a separate gate, not this task's concern) and
    /// then driven by an unprivileged, editor-capability-less caller.
    const P2: &str = "/etc/rc/editorcaptest/create/S00-foo.kai";

    /// Bind a fresh, unprivileged context and return a caller for it.
    /// `register_context` seeds a broad test loadout (`*`, `facade:*`,
    /// admin, rc-write, drive/fork/drift/transport/operator/config-write) so
    /// ordinary kj verb-mechanics tests don't trip the gates — it does NOT
    /// include `editor` (like every authority, deliberately not implied by
    /// `*`), so this is already the right fixture for "denied without
    /// editor" without any further narrowing.
    async fn uncapable_caller(d: &KjDispatcher) -> (kaijutsu_types::ContextId, KjCaller) {
        let ctx = register_context(d, None, None, kaijutsu_types::PrincipalId::system());
        let caller = caller_with_context(ctx);
        (ctx, caller)
    }

    /// Grant `Capability::Editor` on `ctx`, writing straight through
    /// `KernelDb` — the same store `require_cap` reads directly
    /// (`crates/kaijutsu-kernel/src/kj/mod.rs`'s `require_cap`, not the
    /// broker's cache) — because this harness never wires the broker's own
    /// KernelDb handle, so `broker().set_binding()`'s persistence step would
    /// silently no-op here.
    fn grant_editor(d: &KjDispatcher, ctx: kaijutsu_types::ContextId) {
        let mut db = d.kernel_db().lock();
        let mut binding = db.get_context_binding(ctx).unwrap().unwrap_or_default();
        binding.grant(crate::mcp::Capability::Editor);
        db.upsert_context_binding(ctx, &binding).unwrap();
    }

    #[tokio::test]
    async fn save_denied_without_editor_capability_while_open_and_edits_pass() {
        let d = test_dispatcher_rc().await;
        let admin = test_caller();
        let s = |v: &str| v.to_string();
        d.dispatch(&[s("rc"), s("add"), s(P2), s("--content"), s("hello")], &admin)
            .await;

        let (_ctx, caller) = uncapable_caller(&d).await;

        // Opening is ungated — no capability required at all.
        let opened = d.dispatch(&[s("editor"), s("open"), s(P2)], &caller).await;
        let id = session_of(&opened);

        // A plain edit (no write verb) is ungated too.
        let edited = d
            .dispatch(&[s("editor"), s("keys"), id.to_string(), s("iX<Esc>")], &caller)
            .await;
        assert!(
            !matches!(edited, KjResult::Err(_)),
            "a plain edit must not require the editor capability: {edited:?}"
        );

        // `kj editor save` is denied the same way.
        let denied_save = d
            .dispatch(&[s("editor"), s("save"), id.to_string()], &caller)
            .await;
        assert!(
            matches!(denied_save, KjResult::Err(_)),
            "save must be denied without the editor capability: {denied_save:?}"
        );
    }

    #[tokio::test]
    async fn keys_write_and_save_succeed_with_editor_capability() {
        let d = test_dispatcher_rc().await;
        let admin = test_caller();
        let s = |v: &str| v.to_string();
        d.dispatch(&[s("rc"), s("add"), s(P2), s("--content"), s("hello")], &admin)
            .await;

        let (ctx, caller) = uncapable_caller(&d).await;
        grant_editor(&d, ctx);

        let opened = d.dispatch(&[s("editor"), s("open"), s(P2)], &caller).await;
        let id = session_of(&opened);

        let saved = d
            .dispatch(&[s("editor"), s("save"), id.to_string()], &caller)
            .await;
        assert!(!matches!(saved, KjResult::Err(_)), "save should succeed: {saved:?}");

        d.dispatch(&[s("editor"), s("keys"), id.to_string(), s("iX<Esc>")], &caller)
            .await;
        let written = d
            .dispatch(&[s("editor"), s("keys"), id.to_string(), s(":w<CR>")], &caller)
            .await;
        assert!(
            !matches!(written, KjResult::Err(_)),
            "`:w` should succeed with the editor capability: {written:?}"
        );
    }
}
