use std::sync::Arc;

use async_trait::async_trait;
use clap::CommandFactory;
use kaijutsu_index::{Embedder, EmbeddingPurpose, IndexConfig, IndexError, SemanticIndex};
use kaijutsu_types::{ContextId, ContextState, PrincipalId, SessionId};

use super::super::effect::{Effect, classify};
use super::super::test_helpers::test_dispatcher;
use super::super::{KjCaller, KjDispatcher, KjResult};
use super::*;
use crate::kernel_db::ContextRow;
use crate::mcp::{Capability, ContextToolBinding};

fn s(x: &str) -> String {
    x.to_string()
}

fn caller_as(principal_id: PrincipalId, context_id: Option<ContextId>) -> KjCaller {
    KjCaller {
        principal_id,
        actor_id: principal_id,
        reviewer_id: None,
        context_id,
        session_id: SessionId::new(),
        confirmed: false,
        rc_depth: 0,
        privileged: false,
        cancel: tokio_util::sync::CancellationToken::new(),
    }
}

fn worker_binding() -> ContextToolBinding {
    let mut b = ContextToolBinding::new();
    b.grant(Capability::Facade("shell_write".into()));
    b.grant(Capability::Drift);
    b.grant(Capability::Fork);
    b
}

fn seed_root(d: &KjDispatcher) -> (PrincipalId, ContextId) {
    let db = d.kernel_db().lock();
    if let Ok(Some(ch)) = db.get_character_by_name("amy")
        && let Some(root_ctx) = ch.root_ctx
    {
        return (ch.principal_id, root_ctx);
    }
    db.seed_test_root_console().expect("seed root console")
}

async fn create_character(d: &KjDispatcher, name: &str, root: bool) -> PrincipalId {
    if let Ok(Some(existing)) = d.kernel_db().lock().get_character_by_name(name) {
        return existing.principal_id;
    }
    let principal_id = PrincipalId::new();
    d.kernel_db()
        .lock()
        .insert_character(&crate::kernel_db::CharacterRow {
            principal_id,
            name: name.to_string(),
            created_at: now_millis() as i64,
            retired_at: None,
            handoff_ctx: None,
            root_ctx: None,
            root,
            rc_dir: Some(if root { "person" } else { "coder" }.into()),
            console_ctx: None,
            mcp_hub_ctx: None,
        })
        .expect("insert character");
    principal_id
}

fn register_bound_context(
    d: &KjDispatcher,
    label: &str,
    performer: PrincipalId,
    senpai_ctx: Option<ContextId>,
    binding: &ContextToolBinding,
) -> ContextId {
    let ctx = ContextId::new();
    {
        let mut db = d.kernel_db().lock();
        let ws = db.get_or_create_default_workspace(performer).unwrap();
        let row = ContextRow {
            context_id: ctx,
            label: Some(label.to_string()),
            system_prompt: None,
            context_state: ContextState::Live,
            created_at: now_millis() as i64,
            created_by: performer,
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
            origin_host: None,
            performer: Some(performer),
            senpai_ctx,
            persona: None,
            board_id: None,
        };
        db.insert_placed_context(&row, ws).unwrap();
        db.upsert_context_binding(ctx, binding).unwrap();
    }
    ctx
}

/// Deterministic keyword-based mock embedder for `--semantic` search tests.
struct MockBoardEmbedder;

#[async_trait]
impl Embedder for MockBoardEmbedder {
    async fn embed(&self, text: &str, _purpose: EmbeddingPurpose) -> Result<Vec<f32>, IndexError> {
        let lower = text.to_ascii_lowercase();
        let v = vec![
            if lower.contains("audio") || lower.contains("midi") || lower.contains("synth") {
                1.0
            } else {
                0.01
            },
            if lower.contains("sqlite") || lower.contains("database") || lower.contains("migration")
            {
                1.0
            } else {
                0.01
            },
            if lower.contains("council") || lower.contains("ledger") || lower.contains("gate") {
                1.0
            } else {
                0.01
            },
            0.05,
        ];
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        Ok(v.into_iter().map(|x| x / norm).collect())
    }

    async fn embed_batch(
        &self,
        texts: &[&str],
        purpose: EmbeddingPurpose,
    ) -> Result<Vec<Vec<f32>>, IndexError> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed(t, purpose).await?);
        }
        Ok(out)
    }

    fn dimensions(&self) -> usize {
        4
    }

    fn model_name(&self) -> &str {
        "mock-board-embedder"
    }

    fn revision(&self) -> &str {
        "v1"
    }
}

#[test]
fn board_verbs_classify_read_and_write() {
    for read_argv in [
        vec![s("board"), s("list")],
        vec![s("board"), s("topics"), s(".")],
        vec![s("board"), s("read"), s("./t")],
        vec![s("board"), s("recent")],
        vec![s("board"), s("search"), s("needle")],
        vec![s("board"), s("search"), s("needle"), s("--semantic")],
    ] {
        assert_eq!(
            classify(&read_argv).unwrap(),
            Effect::Read,
            "expected Read for {read_argv:?}"
        );
    }

    for write_argv in [
        vec![s("board"), s("create"), s("notes")],
        vec![s("board"), s("set"), s("notes"), s("--policy"), s("curated")],
        vec![s("board"), s("post"), s("."), s("--title"), s("hi"), s("body")],
        vec![s("board"), s("reply"), s("./hi"), s("more")],
        vec![s("board"), s("resolve"), s("./hi")],
        vec![s("board"), s("reopen"), s("./hi")],
        vec![s("board"), s("archive"), s("./hi")],
        vec![s("board"), s("pin"), s("./hi")],
        vec![s("board"), s("unpin"), s("./hi")],
        vec![s("board"), s("lock"), s("./hi")],
        vec![s("board"), s("unlock"), s("./hi")],
        vec![s("board"), s("hide"), s("./hi#1")],
        vec![s("board"), s("unhide"), s("./hi#1")],
    ] {
        assert_eq!(
            classify(&write_argv).unwrap(),
            Effect::Write,
            "expected Write for {write_argv:?}"
        );
    }
}

#[test]
fn board_help_uses_topic_never_thread() {
    let mut root = BoardArgs::command();
    let root_help = root.render_long_help().to_string();
    assert!(
        !root_help.to_ascii_lowercase().contains("thread"),
        "board --help must use 'topic', never 'thread':\n{root_help}"
    );
    for sub in root.get_subcommands_mut() {
        let sub_help = sub.render_long_help().to_string();
        assert!(
            !sub_help.to_ascii_lowercase().contains("thread"),
            "board {} --help must use 'topic', never 'thread':\n{sub_help}",
            sub.get_name()
        );
    }
}

#[tokio::test]
async fn create_list_post_reply_read_and_backref() {
    let d = test_dispatcher().await;
    let (_root_id, root_console) = seed_root(&d);
    let alice_id = create_character(&d, "alice", false).await;
    let sol_id = create_character(&d, "sol", false).await;
    let alice_ctx = register_bound_context(&d, "alice-work", alice_id, Some(root_console), &worker_binding());
    let sol_ctx = register_bound_context(&d, "sol-work", sol_id, Some(alice_ctx), &worker_binding());
    let alice = caller_as(alice_id, Some(alice_ctx));
    let sol = caller_as(sol_id, Some(sol_ctx));

    // Create a character board for alice.
    let created = d
        .dispatch(
            &[
                s("board"),
                s("create"),
                s("kb"),
                s("--desc"),
                s("shared notes"),
            ],
            &alice,
        )
        .await;
    assert!(created.is_ok(), "create failed: {}", created.message());

    // List boards for alice.
    let listed = d.dispatch(&[s("board"), s("list"), s("alice")], &sol).await;
    assert!(listed.is_ok(), "list failed: {}", listed.message());
    assert!(listed.message().contains("alice/kb"), "{}", listed.message());
    assert!(listed.message().contains("shared notes"), "{}", listed.message());

    // Post a topic on `self/kb` as alice.
    let posted = d
        .dispatch(
            &[
                s("board"),
                s("post"),
                s("self/kb"),
                s("--title"),
                s("Council v2"),
                s("--tag"),
                s("design"),
                s("First"),
                s("opening"),
                s("post"),
            ],
            &alice,
        )
        .await;
    assert!(posted.is_ok(), "post failed: {}", posted.message());
    let short_id = match &posted {
        KjResult::Ok { data: Some(v), .. } => v["short_id"].as_str().unwrap().to_string(),
        other => panic!("expected Ok with data, got {other:?}"),
    };

    // Reply as sol using `<char>/<board>/<topic>` and `--to 1`.
    let replied = d
        .dispatch(
            &[
                s("board"),
                s("reply"),
                s("alice/kb/council-v2"),
                s("--to"),
                s("1"),
                s("Looks"),
                s("solid"),
            ],
            &sol,
        )
        .await;
    assert!(replied.is_ok(), "reply failed: {}", replied.message());

    // Read by hex prefix and verify numbering + backref header + structured JSON.
    let read = d.dispatch(&[s("board"), s("read"), short_id.clone()], &sol).await;
    assert!(read.is_ok(), "read failed: {}", read.message());
    let msg = read.message();
    assert!(msg.contains("#1 @alice"), "{msg}");
    assert!(msg.contains("#2 @sol"), "{msg}");
    assert!(
        msg.contains("[re #1 @alice: \"First opening post\"]"),
        "expected backref header:\n{msg}"
    );
    let KjResult::Ok { data: Some(data), .. } = read else {
        panic!("expected structured data");
    };
    assert_eq!(data["posts"].as_array().unwrap().len(), 2);
    assert_eq!(data["posts"][1]["parent_number"], 1);

    // `--after 1` returns only post #2 (its backref header still cites #1).
    let after = d
        .dispatch(&[s("board"), s("read"), short_id, s("--after"), s("1")], &sol)
        .await;
    assert!(after.is_ok(), "{}", after.message());
    assert!(!after.message().contains("\n#1 @alice"), "{}", after.message());
    assert!(after.message().contains("\n#2 @sol"), "{}", after.message());
    if let KjResult::Ok { data: Some(after_data), .. } = after {
        let posts = after_data["posts"].as_array().unwrap();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0]["number"], 2);
    } else {
        panic!("expected structured data for --after");
    }

    // Topics listing with `--tag`.
    let topics = d
        .dispatch(&[s("board"), s("topics"), s("alice/kb"), s("--tag"), s("design")], &sol)
        .await;
    assert!(topics.is_ok(), "{}", topics.message());
    assert!(topics.message().contains("council-v2"), "{}", topics.message());

    // Recent topics with duration parsing.
    let recent = d
        .dispatch(&[s("board"), s("recent"), s("--board"), s("alice/kb"), s("--since"), s("24h")], &sol)
        .await;
    assert!(recent.is_ok(), "{}", recent.message());
    assert!(recent.message().contains("council-v2"), "{}", recent.message());
}

#[tokio::test]
async fn context_board_lazy_creation_and_senpai_coordination_across_rotate() {
    let d = test_dispatcher().await;
    let (_root_id, root_console) = seed_root(&d);
    let banto_id = create_character(&d, "banto", false).await;
    let terra_a_id = create_character(&d, "terra-a", false).await;
    let terra_b_id = create_character(&d, "terra-b", false).await;

    let banto_ctx = register_bound_context(&d, "banto", banto_id, Some(root_console), &worker_binding());
    let kohai_a_ctx = register_bound_context(&d, "kohai-a", terra_a_id, Some(banto_ctx), &worker_binding());
    let kohai_b_ctx = register_bound_context(&d, "kohai-b", terra_b_id, Some(banto_ctx), &worker_binding());

    let banto = caller_as(banto_id, Some(banto_ctx));
    let kohai_a = caller_as(terra_a_id, Some(kohai_a_ctx));
    let kohai_b = caller_as(terra_b_id, Some(kohai_b_ctx));

    // Reading `.` or `.senpai` before any write returns clean empty result and creates no board.
    let empty_dot = d.dispatch(&[s("board"), s("topics"), s(".")], &banto).await;
    assert!(empty_dot.is_ok(), "{}", empty_dot.message());
    assert!(empty_dot.message().contains("no topics"), "{}", empty_dot.message());
    assert_eq!(d.kernel_db().lock().get_context(banto_ctx).unwrap().unwrap().board_id, None);

    let empty_senpai = d.dispatch(&[s("board"), s("topics"), s(".senpai")], &kohai_a).await;
    assert!(empty_senpai.is_ok(), "{}", empty_senpai.message());
    assert_eq!(d.kernel_db().lock().get_context(banto_ctx).unwrap().unwrap().board_id, None);

    // Kohai A writes to `.senpai` -> lazily creates banto's working board.
    let post_a = d
        .dispatch(
            &[
                s("board"),
                s("post"),
                s(".senpai"),
                s("--title"),
                s("CI Failures"),
                s("flaky test in block_store"),
            ],
            &kohai_a,
        )
        .await;
    assert!(post_a.is_ok(), "kohai_a post failed: {}", post_a.message());
    let board_id = d
        .kernel_db()
        .lock()
        .get_context(banto_ctx)
        .unwrap()
        .unwrap()
        .board_id
        .expect("banto context board must be lazily created");

    // Kohai B replies via `.senpai/ci-failures` and banto reads via `./ci-failures`.
    let reply_b = d
        .dispatch(
            &[s("board"), s("reply"), s(".senpai/ci-failures"), s("reproduced locally")],
            &kohai_b,
        )
        .await;
    assert!(reply_b.is_ok(), "kohai_b reply failed: {}", reply_b.message());

    let banto_read = d.dispatch(&[s("board"), s("read"), s("./ci-failures")], &banto).await;
    assert!(banto_read.is_ok(), "{}", banto_read.message());
    assert!(banto_read.message().contains("flaky test in block_store"), "{}", banto_read.message());
    assert!(banto_read.message().contains("reproduced locally"), "{}", banto_read.message());

    // Simulate rotation: successor carries `board_id`, predecessor archives keeping `board_id`.
    let banto_v2_ctx = register_bound_context(&d, "banto-v2", banto_id, Some(root_console), &worker_binding());
    {
        let db = d.kernel_db().lock();
        db.commit_rotation(banto_ctx, banto_v2_ctx).unwrap();
    }
    let banto_v2 = caller_as(banto_id, Some(banto_v2_ctx));

    // Successor sees the same board via `.` and un-reparented kohai still reaches it via `.senpai`!
    assert_eq!(
        d.kernel_db().lock().get_context(banto_v2_ctx).unwrap().unwrap().board_id,
        Some(board_id)
    );
    let reply_after_rotate = d
        .dispatch(
            &[s("board"), s("reply"), s(".senpai/ci-failures"), s("patch ready")],
            &kohai_a,
        )
        .await;
    assert!(
        reply_after_rotate.is_ok(),
        "kohai_a reply after senpai rotate failed: {}",
        reply_after_rotate.message()
    );
    let v2_read = d.dispatch(&[s("board"), s("read"), s("./ci-failures")], &banto_v2).await;
    assert!(v2_read.is_ok(), "{}", v2_read.message());
    assert!(v2_read.message().contains("patch ready"), "{}", v2_read.message());
}

#[tokio::test]
async fn moderation_hide_lock_resolve_reopen_pin_archive_and_authority() {
    let d = test_dispatcher().await;
    let (root_id, root_ctx) = seed_root(&d);
    let owner_id = create_character(&d, "librarian", false).await;
    let senpai_id = create_character(&d, "senpai", false).await;
    let outsider_id = create_character(&d, "stranger", false).await;

    let senpai_ctx = register_bound_context(&d, "senpai-ctx", senpai_id, Some(root_ctx), &worker_binding());
    let owner_ctx = register_bound_context(&d, "lib-ctx", owner_id, Some(senpai_ctx), &worker_binding());
    let outsider_ctx = register_bound_context(&d, "out-ctx", outsider_id, Some(root_ctx), &worker_binding());

    let owner = caller_as(owner_id, Some(owner_ctx));
    let senpai = caller_as(senpai_id, Some(senpai_ctx));
    let outsider = caller_as(outsider_id, Some(outsider_ctx));
    let root = caller_as(root_id, Some(root_ctx));

    // Outsider cannot create a board for librarian.
    let denied_create = d
        .dispatch(&[s("board"), s("create"), s("notes"), s("--for"), s("librarian")], &outsider)
        .await;
    assert!(!denied_create.is_ok(), "outsider must not create board for another character");

    // Senpai CAN create a board for librarian.
    let senpai_create = d
        .dispatch(&[s("board"), s("create"), s("notes"), s("--for"), s("librarian")], &senpai)
        .await;
    assert!(senpai_create.is_ok(), "senpai create failed: {}", senpai_create.message());

    // Post + reply on open board.
    assert!(
        d.dispatch(
            &[s("board"), s("post"), s("librarian/notes"), s("--title"), s("Topic One"), s("alpha secret")],
            &outsider,
        )
        .await
        .is_ok()
    );
    assert!(
        d.dispatch(
            &[s("board"), s("reply"), s("librarian/notes/topic-one"), s("beta noisy")],
            &outsider,
        )
        .await
        .is_ok()
    );

    // Outsider cannot hide post #2.
    assert!(
        !d.dispatch(&[s("board"), s("hide"), s("librarian/notes/topic-one#2")], &outsider)
            .await
            .is_ok()
    );

    // Owner hides post #2 -> hidden from default read & regex search, visible with --include-hidden.
    let hid = d
        .dispatch(&[s("board"), s("hide"), s("librarian/notes/topic-one#2")], &owner)
        .await;
    assert!(hid.is_ok(), "{}", hid.message());

    let read_default = d
        .dispatch(&[s("board"), s("read"), s("librarian/notes/topic-one")], &outsider)
        .await;
    assert!(!read_default.message().contains("beta noisy"), "{}", read_default.message());

    let read_hidden = d
        .dispatch(
            &[s("board"), s("read"), s("librarian/notes/topic-one"), s("--include-hidden")],
            &outsider,
        )
        .await;
    assert!(read_hidden.message().contains("beta noisy"), "{}", read_hidden.message());
    assert!(read_hidden.message().contains("[hidden]"), "{}", read_hidden.message());

    let search_hidden = d
        .dispatch(&[s("board"), s("search"), s("beta noisy")], &outsider)
        .await;
    assert!(search_hidden.message().contains("no matches"), "{}", search_hidden.message());

    // Unhide restores visibility and regex search hit.
    assert!(
        d.dispatch(&[s("board"), s("unhide"), s("librarian/notes/topic-one#2")], &owner)
            .await
            .is_ok()
    );
    let search_restored = d
        .dispatch(&[s("board"), s("search"), s("beta noisy")], &outsider)
        .await;
    assert!(search_restored.message().contains("beta noisy"), "{}", search_restored.message());

    // Lock / unlock topic.
    assert!(
        d.dispatch(&[s("board"), s("lock"), s("librarian/notes/topic-one")], &owner)
            .await
            .is_ok()
    );
    let locked_reply = d
        .dispatch(&[s("board"), s("reply"), s("librarian/notes/topic-one"), s("blocked")], &owner)
        .await;
    assert!(!locked_reply.is_ok(), "reply to locked topic must fail");
    assert!(
        d.dispatch(&[s("board"), s("unlock"), s("librarian/notes/topic-one")], &owner)
            .await
            .is_ok()
    );
    assert!(
        d.dispatch(&[s("board"), s("reply"), s("librarian/notes/topic-one"), s("unblocked")], &outsider)
            .await
            .is_ok()
    );

    // Pin / unpin / resolve / reopen / archive.
    assert!(d.dispatch(&[s("board"), s("pin"), s("librarian/notes/topic-one")], &owner).await.is_ok());
    assert!(d.dispatch(&[s("board"), s("unpin"), s("librarian/notes/topic-one")], &owner).await.is_ok());
    assert!(d.dispatch(&[s("board"), s("resolve"), s("librarian/notes/topic-one")], &outsider).await.is_ok());
    assert!(d.dispatch(&[s("board"), s("reopen"), s("librarian/notes/topic-one")], &outsider).await.is_ok());
    assert!(!d.dispatch(&[s("board"), s("archive"), s("librarian/notes/topic-one")], &outsider).await.is_ok());
    assert!(d.dispatch(&[s("board"), s("archive"), s("librarian/notes/topic-one")], &owner).await.is_ok());

    // Curated board: owner & senpai publish directly, outsider refused.
    assert!(
        d.dispatch(&[s("board"), s("set"), s("librarian/notes"), s("--policy"), s("curated")], &owner)
            .await
            .is_ok()
    );
    assert!(
        !d.dispatch(
            &[s("board"), s("post"), s("librarian/notes"), s("--title"), s("Denied"), s("body")],
            &outsider,
        )
        .await
        .is_ok()
    );
    assert!(
        d.dispatch(
            &[s("board"), s("post"), s("librarian/notes"), s("--title"), s("Senpai Ok"), s("body")],
            &senpai,
        )
        .await
        .is_ok()
    );

    // Locked board: owner refused, root allowed.
    assert!(
        d.dispatch(&[s("board"), s("set"), s("librarian/notes"), s("--policy"), s("locked")], &owner)
            .await
            .is_ok()
    );
    assert!(
        !d.dispatch(
            &[s("board"), s("post"), s("librarian/notes"), s("--title"), s("Locked Out"), s("body")],
            &owner,
        )
        .await
        .is_ok()
    );
    assert!(
        d.dispatch(
            &[s("board"), s("post"), s("librarian/notes"), s("--title"), s("Root Ok"), s("body")],
            &root,
        )
        .await
        .is_ok()
    );
}

#[tokio::test]
async fn post_and_reply_enforce_32kib_limit() {
    let d = test_dispatcher().await;
    let amy_id = create_character(&d, "amy", true).await;
    let amy = caller_as(amy_id, None);
    assert!(d.dispatch(&[s("board"), s("create"), s("notes")], &amy).await.is_ok());

    let too_big = "a".repeat(MAX_POST_BYTES + 1);
    let post_err = d
        .dispatch(&[s("board"), s("post"), s("notes"), s("--title"), s("Big"), too_big.clone()], &amy)
        .await;
    assert!(!post_err.is_ok(), "oversize post must fail");
    assert!(post_err.message().contains("32 KiB"), "{}", post_err.message());

    let ok_post = d
        .dispatch(&[s("board"), s("post"), s("notes"), s("--title"), s("Small"), s("ok")], &amy)
        .await;
    assert!(ok_post.is_ok(), "{}", ok_post.message());

    let reply_err = d
        .dispatch(&[s("board"), s("reply"), s("notes/small"), too_big], &amy)
        .await;
    assert!(!reply_err.is_ok(), "oversize reply must fail");
    assert!(reply_err.message().contains("32 KiB"), "{}", reply_err.message());
}

#[tokio::test]
async fn semantic_search_fails_when_unconfigured_and_ranks_topics_when_configured() {
    let d = test_dispatcher().await;
    let amy_id = create_character(&d, "amy", true).await;
    let amy = caller_as(amy_id, None);
    assert!(d.dispatch(&[s("board"), s("create"), s("kb")], &amy).await.is_ok());
    assert!(d.dispatch(&[s("board"), s("create"), s("other")], &amy).await.is_ok());

    // Without semantic index configured -> clear failure.
    let unconfigured = d
        .dispatch(&[s("board"), s("search"), s("audio synthesis"), s("--semantic")], &amy)
        .await;
    assert!(!unconfigured.is_ok(), "expected error when semantic index is unconfigured");
    assert!(
        unconfigured.message().contains("not configured"),
        "{}",
        unconfigured.message()
    );

    // Post three topics across two boards.
    let p_audio = d
        .dispatch(
            &[s("board"), s("post"), s("kb"), s("--title"), s("Audio Graph"), s("midi synth audio routing")],
            &amy,
        )
        .await;
    let p_db = d
        .dispatch(
            &[s("board"), s("post"), s("kb"), s("--title"), s("SQLite WAL"), s("sqlite database migration")],
            &amy,
        )
        .await;
    let p_other = d
        .dispatch(
            &[s("board"), s("post"), s("other"), s("--title"), s("Other Audio"), s("audio synth midi")],
            &amy,
        )
        .await;
    assert!(p_audio.is_ok() && p_db.is_ok() && p_other.is_ok());

    let doc_id_of = |res: &KjResult| match res {
        KjResult::Ok { data: Some(v), .. } => {
            ContextId::parse(v["document_id"].as_str().unwrap()).unwrap()
        }
        other => panic!("expected Ok with data, got {other:?}"),
    };
    let audio_id = doc_id_of(&p_audio);
    let db_id = doc_id_of(&p_db);
    let other_id = doc_id_of(&p_other);

    // Install a mock SemanticIndex and index all three topics.
    let tmp = tempfile::tempdir().unwrap();
    let cfg = IndexConfig::new(4, 16 * 1024, tmp.path());
    let index = Arc::new(SemanticIndex::new(cfg, Box::new(MockBoardEmbedder)).unwrap());
    for id in [audio_id, db_id, other_id] {
        let snaps = d.block_store().non_draft_snapshots(id).unwrap();
        index.index_context(id, &snaps).await.unwrap();
    }
    d.set_semantic_index(Some(index));

    // Scoped to `amy/kb`, audio query ranks `audio-graph` first and excludes `other/other-audio`.
    let res = d
        .dispatch(
            &[
                s("board"),
                s("search"),
                s("midi synth"),
                s("--board"),
                s("amy/kb"),
                s("--semantic"),
            ],
            &amy,
        )
        .await;
    assert!(res.is_ok(), "semantic search failed: {}", res.message());
    let first_line = res.message().lines().next().unwrap();
    assert!(first_line.contains("audio-graph"), "expected audio-graph first:\n{}", res.message());
    assert!(!res.message().contains("other-audio"), "must stay scoped to amy/kb:\n{}", res.message());

    // Unscoped semantic search sees both boards.
    let all_res = d
        .dispatch(&[s("board"), s("search"), s("sqlite migration"), s("--semantic")], &amy)
        .await;
    assert!(all_res.is_ok(), "{}", all_res.message());
    assert!(
        all_res.message().lines().next().unwrap().contains("sqlite-wal"),
        "expected sqlite-wal first:\n{}",
        all_res.message()
    );
}

#[test]
fn duration_and_post_ref_parsers_validate_inputs() {
    assert_eq!(parse_duration_ms("60s").unwrap(), 60_000);
    assert_eq!(parse_duration_ms("30m").unwrap(), 1_800_000);
    assert_eq!(parse_duration_ms("24h").unwrap(), 86_400_000);
    assert_eq!(parse_duration_ms("7d").unwrap(), 604_800_000);
    assert_eq!(parse_duration_ms("1w").unwrap(), 604_800_000);
    assert!(parse_duration_ms("0h").is_err());
    assert!(parse_duration_ms("24x").is_err());
    assert!(parse_duration_ms("abc").is_err());

    assert_eq!(parse_post_ref("a1b2c3d4#3").unwrap(), ("a1b2c3d4", 3));
    assert_eq!(parse_post_ref("librarian/kb/topic#1").unwrap(), ("librarian/kb/topic", 1));
    assert!(parse_post_ref("a1b2c3d4#0").is_err());
    assert!(parse_post_ref("a1b2c3d4").is_err());
    assert!(parse_post_ref("#2").is_err());
}
