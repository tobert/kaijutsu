//! `kj editor` — drive the kernel-owned editor sessions over the kj surface.
//!
//! The programmatic face of the in-app editor (`docs/vi.md`): `open` resolves a
//! path to its owning CRDT block and starts a session; `keys` feeds vim input
//! and mirrors the edits onto the block; `state` reads the buffer; `save`/`quit`
//! are `ZZ`/`ZQ`. The Bevy app renders these same kernel sessions, and a model
//! drives them through here — one surface, many hands.

use clap::{Parser, Subcommand};

use super::{clap_help_for, KjCaller, KjDispatcher, KjResult};
use crate::editor::{EditorSessionId, EditorState};

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
    /// Open an editor on a path, binding to the CRDT block that owns it.
    Open {
        /// File or rc/config path to edit (e.g. /etc/rc/coder/create/S00.kai).
        path: String,
    },
    /// Feed vim keys to a session (e.g. "iX<Esc>", "dw", "<C-w>").
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
    /// Checkpoint the buffer as saved (`ZZ`).
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
        let blocks = self.block_store();
        // Record the opener (principal + context) so `fg` and `:r !cmd` work;
        // a caller with no joined context degrades to a headless-style open.
        let opener = caller.context_id.map(|context_id| crate::editor::EditorOpener {
            principal: caller.principal_id,
            context_id,
            session_id: caller.session_id,
        });
        match parsed.command {
            EditorCommand::Open { path } => match kernel
                .editor_open_signaled(&path, blocks, opener)
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
                match kernel.editor_keys(id, &keys, blocks).await {
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
                let id = EditorSessionId::from_u64(session);
                match kernel.editor_save(id) {
                    Ok(st) => KjResult::ok_with_data(
                        format!("session {session}: saved"),
                        state_json(id, &st),
                    ),
                    Err(e) => KjResult::Err(format!("kj editor save: {e}")),
                }
            }
            EditorCommand::Quit { session } => {
                let id = EditorSessionId::from_u64(session);
                match kernel.editor_quit(id, blocks) {
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
    use crate::kj::{KjDispatcher, KjResult};

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
    /// ConfigCrdtFs), and `quit` rolls it back to the opened content.
    #[tokio::test]
    async fn kj_editor_edits_the_rc_doc_and_quit_rolls_back() {
        let d = test_dispatcher_crdt_rc().await;
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
        let d = test_dispatcher_crdt_rc().await;
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
}
