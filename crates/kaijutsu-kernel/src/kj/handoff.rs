//! `kj handoff note|tail` — the handoff log, an ordinary context
//! (`docs/character.md`, "The handoff is an ordinary context", slice 4).
//!
//! Every character gets one context of `context_type = "handoff"`, pointed
//! at by `characters.handoff_ctx`, minted lazily by the first `note` a
//! character's log ever receives. `note` appends a block authored by the
//! CALLER's principal — never the target's — so a message left for another
//! character reads as coming from whoever actually wrote it; the handoff
//! log read by someone else IS the message board. `tail` reads the most
//! recent notes back, each rendered with its author's name and when it
//! landed.
//!
//! `note` is ungated (see `dispatch_handoff`'s doc comment on why this
//! differs from `kj character`'s mutating verbs). `tail` never mints —
//! a character with no notes yet has nothing to read, and `tail`'s whole
//! flag surface must stay incapable of a write to sit in
//! `kj/readonly.rs`'s `READ_ONLY_TABLE`, the same rationale as `kj
//! character list`/`show`.

use clap::{Parser, Subcommand};
use kaijutsu_types::{
    BlockKind, ConsentMode, ContentType, ContextId, ContextState, Role, Status,
};

use super::{KjCaller, KjDispatcher, KjResult, clap_help_for};
use crate::block_store::BlockStoreError;
use crate::kernel_db::{CharacterRow, ContextRow};

/// Hydration window set on a handoff context the first time it gets a
/// block — `docs/character.md`'s "persisted hydration policy so a reader
/// takes a window", sized generously (a handoff log is read in full only
/// rarely, by a proctor doing history, not by an ordinary `tail`).
const HANDOFF_HYDRATION_WINDOW: u32 = 50;

/// Default `tail` window when `--window` is omitted — matches the
/// `S16-handoff.kai` injection width named in `docs/character.md`.
const DEFAULT_TAIL_WINDOW: u32 = 12;

#[derive(Parser, Debug)]
#[command(
    name = "handoff",
    about = "A character's handoff log — the message board read by whoever comes next",
    disable_help_subcommand = true,
    no_binary_name = true
)]
pub(crate) struct HandoffArgs {
    #[command(subcommand)]
    command: HandoffCommand,
}

#[derive(Subcommand, Debug)]
enum HandoffCommand {
    /// Append a note to a handoff log, authored by the caller. Place
    /// `--for` before the text — the note is a trailing variadic argument
    /// and will otherwise swallow a later flag.
    Note {
        /// Whose log to write into; defaults to the caller's own character.
        /// The note is still authored by the caller, not this character.
        #[arg(long = "for")]
        for_character: Option<String>,
        /// Note text (all remaining words joined with spaces).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        text: Vec<String>,
    },
    /// Read the most recent notes from a handoff log.
    Tail {
        /// How many of the most recent notes to show. Default 12.
        #[arg(long)]
        window: Option<u32>,
        /// Whose log to read; defaults to the caller's own character.
        character: Option<String>,
    },
}

impl KjDispatcher {
    /// `note` is left ungated, unlike `kj character create`/`retire`'s
    /// `ConfigWrite` gate: minting or retiring an identity is global,
    /// cross-context administration, but a handoff note is ordinary
    /// authoring — the same shape as `kj block append`, which no capability
    /// gates either. Gating it would make leaving a note for the next
    /// session a privileged act, which the design never asks for.
    pub(crate) async fn dispatch_handoff(&self, argv: &[String], caller: &KjCaller) -> KjResult {
        if argv.is_empty() {
            return clap_help_for::<HandoffArgs>();
        }
        let parsed = match HandoffArgs::try_parse_from(argv) {
            Ok(p) => p,
            Err(e) => {
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp
                        | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                ) {
                    return KjResult::ok_ephemeral(e.to_string(), ContentType::Plain);
                }
                return KjResult::Err(format!("kj handoff: {e}"));
            }
        };

        match parsed.command {
            HandoffCommand::Note { for_character, text } => {
                self.handoff_note(for_character.as_deref(), &text.join(" "), caller)
            }
            HandoffCommand::Tail { window, character } => {
                self.handoff_tail(character.as_deref(), window, caller)
            }
        }
    }

    /// Resolve the caller to the character they play. Never falls back to a
    /// default identity or to `system` — a principal with no sheet row
    /// can't be the implicit "my own log" target, and the fix is always to
    /// mint one, never to guess.
    fn resolve_caller_character(&self, caller: &KjCaller, verb: &str) -> Result<CharacterRow, String> {
        let db = self.kernel_db().lock();
        match db.get_character(caller.principal_id) {
            Ok(Some(row)) => Ok(row),
            Ok(None) => Err(format!(
                "kj handoff {verb}: principal {} has no character — \
                 `kj character create <name>` first",
                caller.principal_id.short()
            )),
            Err(e) => Err(format!("kj handoff {verb}: {e}")),
        }
    }

    fn handoff_note(&self, for_character: Option<&str>, text: &str, caller: &KjCaller) -> KjResult {
        if text.is_empty() {
            return KjResult::Err("kj handoff note: note text must not be empty".to_string());
        }
        let caller_char = match self.resolve_caller_character(caller, "note") {
            Ok(row) => row,
            Err(e) => return KjResult::Err(e),
        };

        let target = match for_character {
            Some(name) => {
                let db = self.kernel_db().lock();
                match db.get_character_by_name(name) {
                    Ok(Some(row)) => row,
                    Ok(None) => {
                        return KjResult::Err(format!(
                            "kj handoff note: no character named '{name}' — \
                             `kj character list` to see who exists"
                        ));
                    }
                    Err(e) => return KjResult::Err(format!("kj handoff note: {e}")),
                }
            }
            None => caller_char,
        };

        // A retired character's log is readable history, not a live inbox
        // (`docs/character.md`, "The handoff is an ordinary context") —
        // `tail` still works; only appending is refused.
        if target.retired_at.is_some() {
            return KjResult::Err(format!(
                "kj handoff note: {} is retired — its handoff log is read-only \
                 (`kj handoff tail {}` still works)",
                target.name, target.name
            ));
        }

        let ctx = match self.get_or_create_handoff_ctx(&target) {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj handoff note: {e}")),
        };

        let block_id = match self.block_store().insert_block_as(
            ctx,
            None,
            None,
            Role::System,
            BlockKind::Notification,
            text.to_string(),
            Status::Done,
            ContentType::Plain,
            Some(caller.principal_id),
        ) {
            Ok(id) => id,
            Err(e) => return KjResult::Err(format!("kj handoff note: {e}")),
        };

        // The hydration window can't be set at context-creation time — an
        // empty context has no block to anchor a prefix marker on
        // (`kj context hydrate`'s own guard against exactly this). Set it
        // here instead, the first time a log gets content, regardless of
        // whether the context itself was minted just now or by an earlier
        // `tail` that found nothing to read.
        {
            let db = self.kernel_db().lock();
            match db.get_hydration_policy(ctx) {
                Ok(None) => {
                    if let Err(e) = db.set_hydration_policy(ctx, block_id, HANDOFF_HYDRATION_WINDOW)
                    {
                        tracing::warn!("kj handoff note: failed to set hydration policy: {e}");
                    }
                }
                Ok(Some(_)) => {}
                Err(e) => {
                    tracing::warn!("kj handoff note: failed to read hydration policy: {e}");
                }
            }
        }

        KjResult::ok(format!("noted to {}'s handoff", target.name))
    }

    fn handoff_tail(&self, character: Option<&str>, window: Option<u32>, caller: &KjCaller) -> KjResult {
        let caller_char = match self.resolve_caller_character(caller, "tail") {
            Ok(row) => row,
            Err(e) => return KjResult::Err(e),
        };

        let target = match character {
            Some(name) => {
                let db = self.kernel_db().lock();
                match db.get_character_by_name(name) {
                    Ok(Some(row)) => row,
                    Ok(None) => {
                        return KjResult::Err(format!(
                            "kj handoff tail: no character named '{name}' — \
                             `kj character list` to see who exists"
                        ));
                    }
                    Err(e) => return KjResult::Err(format!("kj handoff tail: {e}")),
                }
            }
            None => caller_char,
        };

        // `tail` never mints — a target with no log yet has nothing to
        // read, and this verb's whole flag surface must stay incapable of
        // a write (`kj/readonly.rs`'s `READ_ONLY_TABLE` carries `handoff
        // tail`; only `note` calls `get_or_create_handoff_ctx`).
        let Some(ctx) = target.handoff_ctx else {
            return KjResult::ok_with_data(
                format!("{} has no handoff notes yet", target.name),
                serde_json::Value::Array(Vec::new()),
            );
        };

        let window = window.unwrap_or(DEFAULT_TAIL_WINDOW) as usize;
        let blocks = match self.block_store().block_snapshots(ctx) {
            Ok(b) => b,
            Err(e) => return KjResult::Err(format!("kj handoff tail: {e}")),
        };
        let start = blocks.len().saturating_sub(window);
        let tail = &blocks[start..];

        if tail.is_empty() {
            return KjResult::ok_with_data(
                format!("{} has no handoff notes yet", target.name),
                serde_json::Value::Array(Vec::new()),
            );
        }

        let db = self.kernel_db().lock();
        let lines: Vec<String> = tail
            .iter()
            .map(|b| {
                let author = db.name_for(b.author());
                let when = super::format::format_timestamp(b.created_at as i64);
                format!("[{when}] {author}: {}", b.content)
            })
            .collect();
        let data = serde_json::Value::Array(
            tail.iter()
                .map(|b| serde_json::Value::String(b.id.to_key()))
                .collect(),
        );
        KjResult::ok_with_data(lines.join("\n"), data)
    }

    /// Get `target`'s handoff context, minting it on first need. The whole
    /// check-then-create sequence runs under one `KernelDb` lock so two
    /// concurrent callers for the same character can't each observe
    /// `handoff_ctx = None` and mint two contexts for one sheet row — the
    /// `target` passed in may be stale (read before this call took the
    /// lock), so the row is re-read fresh once the lock is held.
    fn get_or_create_handoff_ctx(&self, target: &CharacterRow) -> Result<ContextId, String> {
        if let Some(ctx) = target.handoff_ctx {
            return Ok(ctx);
        }

        let new_id = ContextId::new();
        let db = self.kernel_db().lock();
        match db.get_character(target.principal_id) {
            Ok(Some(row)) => {
                if let Some(ctx) = row.handoff_ctx {
                    return Ok(ctx);
                }
            }
            Ok(None) => {
                return Err(format!(
                    "character '{}' vanished while creating its handoff context",
                    target.name
                ));
            }
            Err(e) => return Err(e.to_string()),
        }

        let default_ws = db
            .get_or_create_default_workspace(target.principal_id)
            .map_err(|e| e.to_string())?;
        let label = format!("handoff/{}", target.name);
        let row = ContextRow {
            context_id: new_id,
            label: Some(label.clone()),
            provider: None,
            model: None,
            system_prompt: None,
            consent_mode: ConsentMode::Collaborative,
            context_state: ContextState::Live,
            context_type: "handoff".to_string(),
            created_at: kaijutsu_types::now_millis() as i64,
            created_by: target.principal_id,
            forked_from: None,
            fork_kind: None,
            archived_at: None,
            workspace_id: None,
            preset_id: None,
            concluded_at: None,
            last_activity_at: None,
            promoted_at: None,
            demoted_at: None,
            paused_at: None,
            cast_id: None,
            origin_host: None,
            played_by: Some(target.principal_id),
        };
        db.insert_context_with_document(&row, default_ws)
            .map_err(|e| e.to_string())?;
        db.set_character_handoff_ctx(target.principal_id, Some(new_id))
            .map_err(|e| e.to_string())?;
        drop(db);

        // `insert_context_with_document` commits the KernelDb document row
        // but doesn't seed the in-memory BlockStore — the same gap
        // `kj/lifecycle.rs`'s rc runner papers over before its first block
        // write. `DocumentAlreadyExists` means another path already seeded
        // it; anything else is a real failure.
        match self.block_store().create_document(
            new_id,
            kaijutsu_types::DocKind::Conversation,
            None,
        ) {
            Ok(()) | Err(BlockStoreError::DocumentAlreadyExists(_)) => {}
            Err(e) => return Err(format!("failed to seed handoff document: {e}")),
        }

        if let Err(e) = self
            .drift_router()
            .write()
            .register(new_id, Some(&label), None, target.principal_id)
        {
            return Err(format!("failed to register handoff context: {e}"));
        }

        Ok(new_id)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::test_dispatcher;
    use super::super::KjResult;
    use kaijutsu_types::{PrincipalId, SessionId};

    fn s(x: &str) -> String {
        x.to_string()
    }

    /// A caller carrying `principal_id`, no active context — `note`/`tail`
    /// resolve entirely from identity and a target name, never from
    /// `context_id`.
    fn caller_as(principal_id: PrincipalId) -> super::super::KjCaller {
        super::super::KjCaller {
            principal_id,
            context_id: None,
            session_id: SessionId::new(),
            confirmed: false,
            rc_depth: 0,
            privileged: false,
        }
    }

    /// Create a character and return a caller whose principal IS that
    /// character — the shape every `note`/`tail` test needs, since a
    /// character's principal is minted independently of any auth binding
    /// in this slice.
    async fn create_and_play(d: &super::super::KjDispatcher, name: &str) -> super::super::KjCaller {
        let admin = super::super::test_helpers::test_caller();
        let result = d.dispatch(&[s("character"), s("create"), s(name)], &admin).await;
        let KjResult::Ok { data: Some(serde_json::Value::Object(obj)), .. } = result else {
            panic!("expected character create to succeed, got {result:?}");
        };
        let hex = obj["principal_id"].as_str().unwrap();
        let principal = PrincipalId::parse(hex).expect("valid principal hex");
        caller_as(principal)
    }

    /// `note` then `tail` round-trips the text.
    #[tokio::test]
    async fn note_then_tail_round_trips_text() {
        let d = test_dispatcher().await;
        let caller = create_and_play(&d, "hajime").await;

        let noted = d
            .dispatch(&[s("handoff"), s("note"), s("left"), s("the"), s("build"), s("green")], &caller)
            .await;
        assert!(matches!(noted, KjResult::Ok { .. }), "{noted:?}");

        let tailed = d.dispatch(&[s("handoff"), s("tail")], &caller).await;
        let KjResult::Ok { message, .. } = tailed else {
            panic!("expected Ok, got {tailed:?}");
        };
        assert!(
            message.contains("left the build green"),
            "expected the note text in the tail, got: {message}"
        );
        assert!(message.contains("hajime"), "expected the author's name, got: {message}");
    }

    /// A caller whose principal has no character fails loudly, and the
    /// message names `kj character create`.
    #[tokio::test]
    async fn note_from_an_unmapped_principal_fails_loudly() {
        let d = test_dispatcher().await;
        let caller = caller_as(PrincipalId::new());
        let result = d.dispatch(&[s("handoff"), s("note"), s("hello")], &caller).await;
        let KjResult::Err(msg) = result else {
            panic!("expected Err, got {result:?}");
        };
        assert!(
            msg.contains("kj character create"),
            "expected the message to point at the fix, got: {msg}"
        );
    }

    /// The handoff context is created exactly once — two notes land in ONE
    /// context, not two.
    #[tokio::test]
    async fn handoff_context_is_created_exactly_once() {
        let d = test_dispatcher().await;
        let caller = create_and_play(&d, "hajime").await;

        d.dispatch(&[s("handoff"), s("note"), s("first")], &caller).await;
        let first_ctx = d
            .kernel_db()
            .lock()
            .get_character(caller.principal_id)
            .unwrap()
            .unwrap()
            .handoff_ctx;
        assert!(first_ctx.is_some(), "the first note must mint a handoff context");

        d.dispatch(&[s("handoff"), s("note"), s("second")], &caller).await;
        let second_ctx = d
            .kernel_db()
            .lock()
            .get_character(caller.principal_id)
            .unwrap()
            .unwrap()
            .handoff_ctx;
        assert_eq!(first_ctx, second_ctx, "a second note must land in the SAME context");

        let blocks = d.block_store().block_snapshots(first_ctx.unwrap()).unwrap();
        assert_eq!(blocks.len(), 2, "both notes must be in that one context");
    }

    /// `--for <other>` lands in the other character's log, authored by the
    /// CALLER's principal, not the target's.
    #[tokio::test]
    async fn note_for_another_character_is_authored_by_the_caller() {
        let d = test_dispatcher().await;
        let writer = create_and_play(&d, "amy").await;
        let reader = create_and_play(&d, "kaijutsu-lead").await;

        let result = d
            .dispatch(
                &[s("handoff"), s("note"), s("--for"), s("kaijutsu-lead"), s("morning"), s("digest")],
                &writer,
            )
            .await;
        assert!(matches!(result, KjResult::Ok { .. }), "{result:?}");

        let lead_ctx = d
            .kernel_db()
            .lock()
            .get_character_by_name("kaijutsu-lead")
            .unwrap()
            .unwrap()
            .handoff_ctx
            .expect("kaijutsu-lead's handoff context must exist");
        let blocks = d.block_store().block_snapshots(lead_ctx).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0].author(),
            writer.principal_id,
            "the note in kaijutsu-lead's log must be authored by amy, the caller"
        );

        // Never authored as the target: writing to someone else's log must
        // not silently claim to speak as them.
        assert_ne!(blocks[0].author(), reader.principal_id);
    }

    /// `tail --window N` returns at most N, the newest N when there are more.
    #[tokio::test]
    async fn tail_window_caps_the_result() {
        let d = test_dispatcher().await;
        let caller = create_and_play(&d, "hajime").await;
        for i in 0..5 {
            d.dispatch(&[s("handoff"), s("note"), format!("note {i}")], &caller).await;
        }

        let result = d
            .dispatch(&[s("handoff"), s("tail"), s("--window"), s("2")], &caller)
            .await;
        let KjResult::Ok { data: Some(serde_json::Value::Array(ids)), message, .. } = result else {
            panic!("expected Ok with a data array, got {result:?}");
        };
        assert_eq!(ids.len(), 2, "must cap at the requested window");
        assert!(message.contains("note 3") && message.contains("note 4"), "expected the newest two, got: {message}");
        assert!(!message.contains("note 0"), "must not include an older note past the window");
    }

    /// `note` to a retired character refuses.
    #[tokio::test]
    async fn note_to_a_retired_character_refuses() {
        let d = test_dispatcher().await;
        let target = create_and_play(&d, "hajime").await;
        // Retire directly through KernelDb — `kj character retire` lives in
        // a different file this slice doesn't touch, and the sheet-level
        // retire is all this test needs.
        assert!(
            d.kernel_db()
                .lock()
                .retire_character(target.principal_id, kaijutsu_types::now_millis() as i64)
                .unwrap()
        );

        let writer = create_and_play(&d, "amy").await;
        let result = d
            .dispatch(
                &[s("handoff"), s("note"), s("--for"), s("hajime"), s("too"), s("late")],
                &writer,
            )
            .await;
        assert!(matches!(result, KjResult::Err(_)), "expected a refusal, got {result:?}");
    }

    /// A retired character's handoff log is still readable by `tail`.
    #[tokio::test]
    async fn tail_reads_a_retired_characters_log() {
        let d = test_dispatcher().await;
        let target = create_and_play(&d, "hajime").await;
        d.dispatch(&[s("handoff"), s("note"), s("before"), s("retiring")], &target).await;
        d.kernel_db()
            .lock()
            .retire_character(target.principal_id, kaijutsu_types::now_millis() as i64)
            .unwrap();

        let reader = create_and_play(&d, "amy").await;
        let result = d
            .dispatch(&[s("handoff"), s("tail"), s("hajime")], &reader)
            .await;
        let KjResult::Ok { message, .. } = result else {
            panic!("expected Ok, got {result:?}");
        };
        assert!(message.contains("before retiring"), "got: {message}");
    }
}
