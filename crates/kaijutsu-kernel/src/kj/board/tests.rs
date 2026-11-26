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

fn senpai_binding() -> ContextToolBinding {
    let mut b = worker_binding();
    b.grant(Capability::Senpai);
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
        vec![s("board"), s("edit"), s("./hi#1"), s("updated")],
        vec![s("board"), s("resolve"), s("./hi")],
        vec![s("board"), s("reopen"), s("./hi")],
        vec![s("board"), s("archive"), s("./hi")],
        vec![s("board"), s("pin"), s("./hi")],
        vec![s("board"), s("unpin"), s("./hi")],
        vec![s("board"), s("lock"), s("./hi")],
        vec![s("board"), s("unlock"), s("./hi")],
        vec![s("board"), s("hide"), s("./hi#1")],
        vec![s("board"), s("unhide"), s("./hi#1")],
        vec![s("board"), s("promote"), s("./hi"), s("--to"), s("notes")],
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
    // Reopen archived topic by slug (not only hex ID), then archive again for the rest of the test.
    let reopen_by_slug = d
        .dispatch(&[s("board"), s("reopen"), s("librarian/notes/topic-one")], &owner)
        .await;
    assert!(reopen_by_slug.is_ok(), "reopen archived topic by slug failed: {}", reopen_by_slug.message());
    assert!(d.dispatch(&[s("board"), s("archive"), s("librarian/notes/topic-one")], &owner).await.is_ok());

    // Curated board: owner & senpai publish directly, outsider is held with an ask.
    assert!(
        d.dispatch(&[s("board"), s("set"), s("librarian/notes"), s("--policy"), s("curated")], &owner)
            .await
            .is_ok()
    );
    let outsider_curated = d
        .dispatch(
            &[s("board"), s("post"), s("librarian/notes"), s("--title"), s("Held Topic"), s("body")],
            &outsider,
        )
        .await;
    assert!(outsider_curated.is_ok(), "outsider post to curated board should hold: {}", outsider_curated.message());
    if let KjResult::Ok { data: Some(ref v), .. } = outsider_curated {
        assert_eq!(v["held"], true);
        assert!(v["ask_id"].as_str().is_some());
    } else {
        panic!("expected structured data with held=true, got {outsider_curated:?}");
    }
    let senpai_curated = d
        .dispatch(
            &[s("board"), s("post"), s("librarian/notes"), s("--title"), s("Senpai Ok"), s("body")],
            &senpai,
        )
        .await;
    assert!(senpai_curated.is_ok(), "{}", senpai_curated.message());
    if let KjResult::Ok { data: Some(ref v), .. } = senpai_curated {
        assert_eq!(v["held"], false);
    }

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
    assert!(parse_duration_ms("24é").is_err());
    assert!(parse_duration_ms("é").is_err());

    assert_eq!(parse_post_ref("a1b2c3d4#3").unwrap(), ("a1b2c3d4", 3));
    assert_eq!(parse_post_ref("librarian/kb/topic#1").unwrap(), ("librarian/kb/topic", 1));
    assert!(parse_post_ref("a1b2c3d4#0").is_err());
    assert!(parse_post_ref("a1b2c3d4").is_err());
    assert!(parse_post_ref("#2").is_err());
}

#[tokio::test]
async fn curated_board_held_post_ledger_allow_and_deny_lifecycle() {
    let d = test_dispatcher().await;
    let (root_id, root_ctx) = seed_root(&d);
    let lib_id = create_character(&d, "librarian", false).await;
    let senpai_id = create_character(&d, "senpai", false).await;
    let writer_id = create_character(&d, "writer", false).await;

    let senpai_ctx = register_bound_context(&d, "senpai-ctx", senpai_id, Some(root_ctx), &senpai_binding());
    let lib_ctx = register_bound_context(&d, "lib-ctx", lib_id, Some(senpai_ctx), &senpai_binding());
    let writer_ctx = register_bound_context(&d, "writer-ctx", writer_id, Some(root_ctx), &worker_binding());

    let lib = caller_as(lib_id, Some(lib_ctx));
    let senpai = caller_as(senpai_id, Some(senpai_ctx));
    let writer = caller_as(writer_id, Some(writer_ctx));
    let root = caller_as(root_id, Some(root_ctx));

    assert!(
        d.dispatch(
            &[s("board"), s("create"), s("kb"), s("--policy"), s("curated")],
            &lib,
        )
        .await
        .is_ok()
    );

    // Owner, senpai, and root publish directly without opening a ledger ask.
    for (who, title) in [(&lib, "Owner Direct"), (&senpai, "Senpai Direct"), (&root, "Root Direct")] {
        let res = d
            .dispatch(
                &[s("board"), s("post"), s("librarian/kb"), s("--title"), s(title), s("direct body")],
                who,
            )
            .await;
        assert!(res.is_ok(), "{title} failed: {}", res.message());
        let KjResult::Ok { data: Some(data), .. } = res else {
            panic!("expected data for {title}");
        };
        assert_eq!(data["held"], false, "{title} must publish directly");
    }
    let pending_before = d.kernel_db().lock().list_pending_asks().unwrap();
    assert!(pending_before.is_empty(), "direct publishes must open no asks");

    // Non-owner posts to curated board -> held (Status::Pending + excluded=true) + ledger ask.
    let held_post = d
        .dispatch(
            &[
                s("board"),
                s("post"),
                s("librarian/kb"),
                s("--title"),
                s("Proposed Entry"),
                s("unique_curated_needle"),
            ],
            &writer,
        )
        .await;
    assert!(held_post.is_ok(), "held post failed: {}", held_post.message());
    let held_msg = held_post.message().to_string();
    let KjResult::Ok { data: Some(held_data), .. } = held_post else {
        panic!("expected structured data");
    };
    assert_eq!(held_data["held"], true);
    let ask_id = held_data["ask_id"].as_str().unwrap().to_string();
    let doc_id = ContextId::parse(held_data["document_id"].as_str().unwrap()).unwrap();
    let block_key = held_data["block_id"].as_str().unwrap().to_string();
    assert!(held_msg.contains(&ask_id), "message must name ask_id:\n{held_msg}");
    assert!(
        held_msg.contains(&format!("kj ledger allow '{ask_id}'")),
        "message must give allow hint:\n{held_msg}"
    );

    // Underlying block is Status::Pending + excluded=true.
    let snaps = d.block_store().non_draft_snapshots(doc_id).unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].status, Status::Pending);
    assert!(snaps[0].excluded);

    // Held-only topic is hidden from default topics, recent, read, and search.
    let topics_default = d.dispatch(&[s("board"), s("topics"), s("librarian/kb")], &writer).await;
    assert!(!topics_default.message().contains("proposed-entry"), "{}", topics_default.message());
    let topics_hidden = d
        .dispatch(&[s("board"), s("topics"), s("librarian/kb"), s("--include-hidden")], &writer)
        .await;
    assert!(topics_hidden.message().contains("proposed-entry"), "{}", topics_hidden.message());

    let recent_default = d
        .dispatch(&[s("board"), s("recent"), s("--board"), s("librarian/kb")], &writer)
        .await;
    assert!(!recent_default.message().contains("proposed-entry"), "{}", recent_default.message());
    let recent_hidden = d
        .dispatch(
            &[s("board"), s("recent"), s("--board"), s("librarian/kb"), s("--include-hidden")],
            &writer,
        )
        .await;
    assert!(recent_hidden.message().contains("proposed-entry"), "{}", recent_hidden.message());

    let read_before = d
        .dispatch(&[s("board"), s("read"), s("librarian/kb/proposed-entry")], &writer)
        .await;
    assert!(!read_before.message().contains("unique_curated_needle"), "{}", read_before.message());

    let search_before = d
        .dispatch(&[s("board"), s("search"), s("unique_curated_needle")], &writer)
        .await;
    assert!(search_before.message().contains("no matches"), "{}", search_before.message());

    // `kj ledger show` reports `board_post` block id and `held` state.
    let show_held = d.dispatch(&[s("ledger"), s("show"), ask_id.clone()], &lib).await;
    assert!(show_held.is_ok(), "{}", show_held.message());
    assert!(show_held.message().contains(&block_key), "{}", show_held.message());
    assert!(show_held.message().contains("board:      held"), "{}", show_held.message());
    if let KjResult::Ok { data: Some(ref v), .. } = show_held {
        assert_eq!(v["board"]["state"], "held");
        assert_eq!(v["board"]["block_id"], block_key);
    }

    // `kj board unhide` refuses to bypass review on a pending held post.
    let unhide_err = d
        .dispatch(&[s("board"), s("unhide"), s("librarian/kb/proposed-entry#1")], &lib)
        .await;
    assert!(!unhide_err.is_ok(), "unhide must refuse pending held post");
    assert!(
        unhide_err.message().contains("kj ledger allow"),
        "unhide refusal should point to kj ledger allow: {}",
        unhide_err.message()
    );
    // BlockStore's defense-in-depth guard also refuses direct un-exclusion while pending.
    let direct_unexclude = d.block_store().set_excluded(doc_id, &snaps[0].id, false);
    assert!(
        direct_unexclude
            .as_ref()
            .is_err_and(|e| e.to_string().contains("held pending review")),
        "direct set_excluded(false) on pending board post must fail: {direct_unexclude:?}"
    );

    // `--remember` is refused on a board ask, leaving it open.
    let remember_err = d
        .dispatch(
            &[s("ledger"), s("allow"), ask_id.clone(), s("--remember"), s("always")],
            &lib,
        )
        .await;
    assert!(!remember_err.is_ok(), "--remember must be refused on board ask");

    // Board owner allows the ask -> block flips to Done + excluded=false, now visible everywhere.
    let allowed = d.dispatch(&[s("ledger"), s("allow"), ask_id.clone()], &lib).await;
    assert!(allowed.is_ok(), "ledger allow failed: {}", allowed.message());

    let snaps_after = d.block_store().non_draft_snapshots(doc_id).unwrap();
    assert_eq!(snaps_after[0].status, Status::Done);
    assert!(!snaps_after[0].excluded);

    let read_after = d
        .dispatch(&[s("board"), s("read"), s("librarian/kb/proposed-entry")], &writer)
        .await;
    assert!(read_after.message().contains("unique_curated_needle"), "{}", read_after.message());

    let topics_after = d.dispatch(&[s("board"), s("topics"), s("librarian/kb")], &writer).await;
    assert!(topics_after.message().contains("proposed-entry"), "{}", topics_after.message());

    let search_after = d
        .dispatch(&[s("board"), s("search"), s("unique_curated_needle")], &writer)
        .await;
    assert!(search_after.message().contains("unique_curated_needle"), "{}", search_after.message());

    let show_released = d.dispatch(&[s("ledger"), s("show"), ask_id.clone()], &lib).await;
    assert!(show_released.message().contains("board:      released"), "{}", show_released.message());

    // Non-owner reply on curated board -> held -> denied -> dismissed & stays hidden.
    let held_reply = d
        .dispatch(
            &[s("board"), s("reply"), s("librarian/kb/proposed-entry"), s("dismissed_reply_token")],
            &writer,
        )
        .await;
    assert!(held_reply.is_ok(), "{}", held_reply.message());
    let KjResult::Ok { data: Some(reply_data), .. } = held_reply else {
        panic!("expected data for held reply");
    };
    assert_eq!(reply_data["held"], true);
    let reply_ask = reply_data["ask_id"].as_str().unwrap().to_string();

    let denied = d
        .dispatch(
            &[s("ledger"), s("deny"), reply_ask.clone(), s("--reason"), s("off topic")],
            &lib,
        )
        .await;
    assert!(denied.is_ok(), "ledger deny failed: {}", denied.message());

    let snaps_denied = d.block_store().non_draft_snapshots(doc_id).unwrap();
    assert_eq!(snaps_denied.len(), 2);
    assert_eq!(snaps_denied[1].status, Status::Done);
    assert!(snaps_denied[1].excluded);

    let read_denied = d
        .dispatch(&[s("board"), s("read"), s("librarian/kb/proposed-entry")], &writer)
        .await;
    assert!(!read_denied.message().contains("dismissed_reply_token"), "{}", read_denied.message());

    let show_dismissed = d.dispatch(&[s("ledger"), s("show"), reply_ask.clone()], &lib).await;
    assert!(show_dismissed.message().contains("board:      dismissed"), "{}", show_dismissed.message());
    assert!(show_dismissed.message().contains("off topic"), "{}", show_dismissed.message());

    // `kj board unhide` must refuse to republish a denied post.
    let unhide_denied = d
        .dispatch(&[s("board"), s("unhide"), s("librarian/kb/proposed-entry#2")], &lib)
        .await;
    assert!(
        !unhide_denied.is_ok(),
        "unhide must refuse to reverse a denied board post"
    );
    assert!(
        unhide_denied.message().contains(&reply_ask)
            && unhide_denied.message().contains("denied"),
        "unhide refusal must name the ask id and status: {}",
        unhide_denied.message()
    );
    let snaps_still_denied = d.block_store().non_draft_snapshots(doc_id).unwrap();
    assert!(snaps_still_denied[1].excluded, "denied post must remain excluded");
}

#[tokio::test]
async fn promote_copies_visible_posts_sets_promoted_from_and_respects_curated_policy() {
    let d = test_dispatcher().await;
    let (_root_id, root_ctx) = seed_root(&d);
    let lib_id = create_character(&d, "librarian", false).await;
    let dev_id = create_character(&d, "dev", false).await;

    let lib_ctx = register_bound_context(&d, "lib-ctx", lib_id, Some(root_ctx), &senpai_binding());
    let dev_ctx = register_bound_context(&d, "dev-ctx", dev_id, Some(root_ctx), &worker_binding());
    let lib = caller_as(lib_id, Some(lib_ctx));
    let dev = caller_as(dev_id, Some(dev_ctx));

    assert!(
        d.dispatch(&[s("board"), s("create"), s("kb"), s("--policy"), s("curated")], &lib)
            .await
            .is_ok()
    );

    // Create a scratch topic on dev's context board with 3 posts, hiding post #2.
    let src_post = d
        .dispatch(
            &[
                s("board"),
                s("post"),
                s("."),
                s("--title"),
                s("WAL Checkpoints"),
                s("--tag"),
                s("sqlite"),
                s("Opening finding about WAL."),
            ],
            &dev,
        )
        .await;
    assert!(src_post.is_ok(), "{}", src_post.message());
    let KjResult::Ok { data: Some(src_data), .. } = src_post else {
        panic!("expected source data");
    };
    let src_doc_id = ContextId::parse(src_data["document_id"].as_str().unwrap()).unwrap();

    assert!(
        d.dispatch(&[s("board"), s("reply"), s("./wal-checkpoints"), s("secret noise to hide")], &dev)
            .await
            .is_ok()
    );
    assert!(
        d.dispatch(&[s("board"), s("reply"), s("./wal-checkpoints"), s("Confirmed fix in pragma.")], &dev)
            .await
            .is_ok()
    );
    assert!(
        d.dispatch(&[s("board"), s("hide"), s("./wal-checkpoints#2")], &dev)
            .await
            .is_ok()
    );

    // Dev promotes to curated `librarian/kb` -> held + ask + `promoted_from` recorded.
    let promoted = d
        .dispatch(
            &[
                s("board"),
                s("promote"),
                s("./wal-checkpoints"),
                s("--to"),
                s("librarian/kb"),
                s("--title"),
                s("SQLite WAL Guide"),
            ],
            &dev,
        )
        .await;
    assert!(promoted.is_ok(), "promote failed: {}", promoted.message());
    let KjResult::Ok { data: Some(p_data), .. } = promoted else {
        panic!("expected promote data");
    };
    assert_eq!(p_data["held"], true);
    assert_eq!(p_data["promoted_from"], src_doc_id.to_hex());
    let promoted_doc_id = ContextId::parse(p_data["document_id"].as_str().unwrap()).unwrap();
    let ask_id = p_data["ask_id"].as_str().unwrap().to_string();

    let topic_row = d.kernel_db().lock().get_topic(promoted_doc_id).unwrap().unwrap();
    assert_eq!(topic_row.promoted_from, Some(src_doc_id));
    assert_eq!(topic_row.tags, vec!["sqlite".to_string()]);

    // Librarian allows the promoted entry -> visible digest includes #1 and #3, excludes hidden #2.
    assert!(d.dispatch(&[s("ledger"), s("allow"), ask_id], &lib).await.is_ok());
    let read = d
        .dispatch(&[s("board"), s("read"), s("librarian/kb/sqlite-wal-guide")], &dev)
        .await;
    assert!(read.is_ok(), "{}", read.message());
    let body = read.message();
    assert!(body.contains("Opening finding about WAL."), "{body}");
    assert!(body.contains("Confirmed fix in pragma."), "{body}");
    assert!(!body.contains("secret noise to hide"), "hidden post must not be promoted:\n{body}");

    // `--summarize` without a configured `distill` slot fails loudly.
    let no_distill = d
        .dispatch(
            &[
                s("board"),
                s("promote"),
                s("./wal-checkpoints"),
                s("--to"),
                s("librarian/kb"),
                s("--summarize"),
            ],
            &dev,
        )
        .await;
    assert!(!no_distill.is_ok(), "--summarize must fail when no distill slot is configured");
    assert!(
        no_distill.message().contains("distill"),
        "error should name the missing distill slot: {}",
        no_distill.message()
    );
}

#[tokio::test]
async fn promote_summarize_uses_caller_distill_slot() {
    let d = test_dispatcher().await;
    let (_root_id, root_ctx) = seed_root(&d);
    let dev_id = create_character(&d, "dev", false).await;

    // Register a mock backend and bind it to dev's `distill` slot before creating dev_ctx.
    let admin = super::super::test_helpers::test_caller();
    {
        let db = d.kernel_db().lock();
        crate::test_backends::example_backends(&db);
    }
    let set_slot = d
        .dispatch(
            &[
                s("character"),
                s("slot"),
                s("set"),
                s("dev"),
                s("distill"),
                s("anthropic/claude-haiku-4-5"),
                s("--max-tokens"),
                s("512"),
            ],
            &admin,
        )
        .await;
    assert!(set_slot.is_ok(), "character slot set failed: {}", set_slot.message());

    let dev_ctx = ContextId::new();
    {
        let mut db = d.kernel_db().lock();
        let ws = db.get_or_create_default_workspace(dev_id).unwrap();
        let row = ContextRow {
            context_id: dev_ctx,
            label: Some("dev-ctx".to_string()),
            system_prompt: None,
            context_state: ContextState::Live,
            created_at: now_millis() as i64,
            created_by: dev_id,
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
            performer: None,
            senpai_ctx: Some(root_ctx),
            persona: None,
            board_id: None,
        };
        db.insert_placed_context(&row, ws).unwrap();
        db.upsert_context_binding(dev_ctx, &worker_binding()).unwrap();
        db.write_context_character(dev_ctx, dev_id, Some(root_ctx), None).unwrap();
    }
    let dev = caller_as(dev_id, Some(dev_ctx));
    {
        let mut llm = d.kernel().llm().write().await;
        llm.register(
            "anthropic",
            Arc::new(crate::llm::Provider::Mock(crate::llm::MockClient::new(
                "## Distilled Summary\n- Key finding preserved.",
            ))),
        );
    }

    assert!(d.dispatch(&[s("board"), s("create"), s("memory")], &dev).await.is_ok());
    assert!(
        d.dispatch(
            &[s("board"), s("post"), s("."), s("--title"), s("Raw Notes"), s("Long raw discussion.")],
            &dev,
        )
        .await
        .is_ok()
    );

    let promoted = d
        .dispatch(
            &[
                s("board"),
                s("promote"),
                s("./raw-notes"),
                s("--to"),
                s("memory"),
                s("--summarize"),
            ],
            &dev,
        )
        .await;
    assert!(promoted.is_ok(), "promote --summarize failed: {}", promoted.message());

    let read = d.dispatch(&[s("board"), s("read"), s("memory/raw-notes")], &dev).await;
    assert!(read.is_ok(), "{}", read.message());
    assert!(
        read.message().contains("Key finding preserved."),
        "expected distilled summary:\n{}",
        read.message()
    );
}

#[tokio::test]
async fn open_board_rate_nudge_refuses_flood_per_character() {
    let d = test_dispatcher().await;
    let (_root_id, root_ctx) = seed_root(&d);
    let alice_id = create_character(&d, "alice", false).await;
    let bob_id = create_character(&d, "bob", false).await;
    let alice_ctx = register_bound_context(&d, "alice-ctx", alice_id, Some(root_ctx), &worker_binding());
    let bob_ctx = register_bound_context(&d, "bob-ctx", bob_id, Some(root_ctx), &worker_binding());
    let alice = caller_as(alice_id, Some(alice_ctx));
    let bob = caller_as(bob_id, Some(bob_ctx));

    d.board_rate_limiter().configure(3, 60_000);
    assert!(d.dispatch(&[s("board"), s("create"), s("chat")], &alice).await.is_ok());

    for i in 1..=3 {
        let res = d
            .dispatch(
                &[s("board"), s("post"), s("alice/chat"), s("--title"), format!("Post {i}"), s("hi")],
                &alice,
            )
            .await;
        assert!(res.is_ok(), "post {i} should succeed: {}", res.message());
    }

    // 4th post by alice hits the rate limit loudly.
    let flood = d
        .dispatch(
            &[s("board"), s("post"), s("alice/chat"), s("--title"), s("Post 4"), s("flood")],
            &alice,
        )
        .await;
    assert!(!flood.is_ok(), "4th post must be refused by rate limiter");
    assert!(
        flood.message().contains("3 posts per 60s"),
        "rate limit error must name the limit: {}",
        flood.message()
    );

    // Bob has his own window on the same board and succeeds.
    let bob_post = d
        .dispatch(
            &[s("board"), s("post"), s("alice/chat"), s("--title"), s("Bob Post"), s("ok")],
            &bob,
        )
        .await;
    assert!(bob_post.is_ok(), "bob should not be rate-limited by alice: {}", bob_post.message());
}

#[tokio::test]
async fn gate_toml_overrides_curated_and_open_board_policies() {
    use crate::vfs::VfsOps;

    let d = test_dispatcher().await;
    let (_root_id, root_ctx) = seed_root(&d);
    let lib_id = create_character(&d, "librarian", false).await;
    let trusted_id = create_character(&d, "trusted", false).await;
    let noisy_id = create_character(&d, "noisy", false).await;

    let lib_ctx = register_bound_context(&d, "lib-ctx", lib_id, Some(root_ctx), &senpai_binding());
    let trusted_ctx = register_bound_context(&d, "trusted-ctx", trusted_id, Some(root_ctx), &worker_binding());
    let noisy_ctx = register_bound_context(&d, "noisy-ctx", noisy_id, Some(root_ctx), &worker_binding());

    let lib = caller_as(lib_id, Some(lib_ctx));
    let trusted = caller_as(trusted_id, Some(trusted_ctx));
    let noisy = caller_as(noisy_id, Some(noisy_ctx));

    assert!(
        d.dispatch(&[s("board"), s("create"), s("kb"), s("--policy"), s("curated")], &lib)
            .await
            .is_ok()
    );
    assert!(
        d.dispatch(&[s("board"), s("create"), s("open-room"), s("--policy"), s("open")], &lib)
            .await
            .is_ok()
    );

    // Write gate.toml: allow `trusted` direct publish to curated boards, deny `noisy` on open boards (puts into held-ask path).
    let gate_toml = r#"
[character.trusted]
allow = ["kj board post", "kj board reply"]

[character.noisy]
deny = ["kj board post"]
"#;
    let gate_path = kaijutsu_types::paths::config_path(crate::kj::gate_policy::GATE_CONFIG_FILE);
    d.kernel()
        .vfs()
        .write_all(std::path::Path::new(&gate_path), gate_toml.as_bytes())
        .await
        .expect("write gate.toml");

    // `trusted` publishes directly to curated `librarian/kb` without an ask.
    let trusted_post = d
        .dispatch(
            &[s("board"), s("post"), s("librarian/kb"), s("--title"), s("Trusted Note"), s("direct")],
            &trusted,
        )
        .await;
    assert!(trusted_post.is_ok(), "{}", trusted_post.message());
    if let KjResult::Ok { data: Some(ref v), .. } = trusted_post {
        assert_eq!(v["held"], false, "gate.toml allow must publish directly to curated board");
    } else {
        panic!("expected data");
    }

    // `noisy` posting to open `librarian/open-room` is held with a ledger ask.
    let noisy_post = d
        .dispatch(
            &[s("board"), s("post"), s("librarian/open-room"), s("--title"), s("Noisy Post"), s("held on open")],
            &noisy,
        )
        .await;
    assert!(noisy_post.is_ok(), "{}", noisy_post.message());
    let KjResult::Ok { data: Some(noisy_data), .. } = noisy_post else {
        panic!("expected data");
    };
    assert_eq!(noisy_data["held"], true, "gate.toml deny on open board must hold post");
    let ask_id = noisy_data["ask_id"].as_str().unwrap().to_string();

    // Board owner can release it via `kj ledger allow`.
    assert!(d.dispatch(&[s("ledger"), s("allow"), ask_id], &lib).await.is_ok());
    let read = d
        .dispatch(&[s("board"), s("read"), s("librarian/open-room/noisy-post")], &lib)
        .await;
    assert!(read.message().contains("held on open"), "{}", read.message());
}

#[tokio::test]
async fn curated_board_held_posts_are_rate_limited() {
    let d = test_dispatcher().await;
    let (_root_id, root_ctx) = seed_root(&d);
    let lib_id = create_character(&d, "librarian", false).await;
    let writer_id = create_character(&d, "writer", false).await;
    let lib_ctx = register_bound_context(&d, "lib-ctx", lib_id, Some(root_ctx), &senpai_binding());
    let writer_ctx = register_bound_context(&d, "writer-ctx", writer_id, Some(root_ctx), &worker_binding());
    let lib = caller_as(lib_id, Some(lib_ctx));
    let writer = caller_as(writer_id, Some(writer_ctx));

    d.board_rate_limiter().configure(2, 60_000);
    assert!(
        d.dispatch(&[s("board"), s("create"), s("kb"), s("--policy"), s("curated")], &lib)
            .await
            .is_ok()
    );

    for i in 1..=2 {
        let res = d
            .dispatch(
                &[s("board"), s("post"), s("librarian/kb"), s("--title"), format!("Proposal {i}"), s("body")],
                &writer,
            )
            .await;
        assert!(res.is_ok(), "proposal {i} should be held: {}", res.message());
    }
    assert_eq!(d.kernel_db().lock().list_pending_asks().unwrap().len(), 2);

    // 3rd held post hits rate limit and opens no 3rd ask.
    let flood = d
        .dispatch(
            &[s("board"), s("post"), s("librarian/kb"), s("--title"), s("Proposal 3"), s("flood")],
            &writer,
        )
        .await;
    assert!(!flood.is_ok(), "curated board flood must be throttled by rate limiter");
    assert!(flood.message().contains("rate limit"), "{}", flood.message());
    assert_eq!(d.kernel_db().lock().list_pending_asks().unwrap().len(), 2);
}

#[tokio::test]
async fn unprivileged_no_context_caller_is_not_treated_as_root() {
    let d = test_dispatcher().await;
    let (_root_id, root_ctx) = seed_root(&d);
    let lib_id = create_character(&d, "librarian", false).await;
    let stranger_id = create_character(&d, "stranger", false).await;
    let lib_ctx = register_bound_context(&d, "lib-ctx", lib_id, Some(root_ctx), &worker_binding());
    let lib = caller_as(lib_id, Some(lib_ctx));
    let stranger_no_ctx = caller_as(stranger_id, None);

    assert!(
        d.dispatch(&[s("board"), s("create"), s("locked-room"), s("--policy"), s("locked")], &lib)
            .await
            .is_ok()
    );

    let locked_post = d
        .dispatch(
            &[s("board"), s("post"), s("librarian/locked-room"), s("--title"), s("Sneak"), s("body")],
            &stranger_no_ctx,
        )
        .await;
    assert!(
        !locked_post.is_ok(),
        "unprivileged non-root caller with context_id: None must not write to locked board"
    );

    let foreign_create = d
        .dispatch(
            &[s("board"), s("create"), s("hijack"), s("--for"), s("librarian")],
            &stranger_no_ctx,
        )
        .await;
    assert!(
        !foreign_create.is_ok(),
        "unprivileged non-root caller with context_id: None must not create board for another character"
    );
}

#[tokio::test]
async fn board_edit_updates_post_with_policy_and_lock_checks() {
    let d = test_dispatcher().await;
    let (root_id, root_ctx) = seed_root(&d);
    let owner_id = create_character(&d, "alice", false).await;
    let writer_id = create_character(&d, "bob", false).await;
    let outsider_id = create_character(&d, "carol", false).await;
    let owner_ctx = register_bound_context(&d, "alice-ctx", owner_id, Some(root_ctx), &worker_binding());
    let writer_ctx = register_bound_context(&d, "bob-ctx", writer_id, Some(root_ctx), &worker_binding());
    let outsider_ctx = register_bound_context(&d, "carol-ctx", outsider_id, Some(root_ctx), &worker_binding());

    let root = caller_as(root_id, Some(root_ctx));
    let owner = caller_as(owner_id, Some(owner_ctx));
    let writer = caller_as(writer_id, Some(writer_ctx));
    let outsider = caller_as(outsider_id, Some(outsider_ctx));

    assert!(d.dispatch(&[s("board"), s("create"), s("notes")], &owner).await.is_ok());
    assert!(
        d.dispatch(
            &[s("board"), s("post"), s("alice/notes"), s("--title"), s("Guide"), s("v1 text")],
            &writer,
        )
        .await
        .is_ok()
    );

    // Author can edit their own post on an open board.
    let edited = d
        .dispatch(&[s("board"), s("edit"), s("alice/notes/guide#1"), s("v2 text")], &writer)
        .await;
    assert!(edited.is_ok(), "author edit should succeed: {edited:?}");
    let read = d.dispatch(&[s("board"), s("read"), s("alice/notes/guide")], &owner).await;
    assert!(read.message().contains("v2 text"), "{}", read.message());
    assert!(!read.message().contains("v1 text"), "{}", read.message());

    // Outsider cannot edit someone else's post.
    let denied = d
        .dispatch(&[s("board"), s("edit"), s("alice/notes/guide#1"), s("hijack")], &outsider)
        .await;
    assert!(!denied.is_ok(), "outsider edit must be refused");

    // Board owner (moderator) can edit another author's post.
    let mod_edit = d
        .dispatch(&[s("board"), s("edit"), s("alice/notes/guide#1"), s("v3 curated")], &owner)
        .await;
    assert!(mod_edit.is_ok(), "board owner edit should succeed: {mod_edit:?}");

    // Empty body and missing post number fail cleanly.
    let empty = d
        .dispatch(&[s("board"), s("edit"), s("alice/notes/guide#1"), s("   ")], &owner)
        .await;
    assert!(!empty.is_ok(), "empty edit must fail");
    let missing = d
        .dispatch(&[s("board"), s("edit"), s("alice/notes/guide#99"), s("nope")], &owner)
        .await;
    assert!(!missing.is_ok(), "missing post edit must fail");

    // Locked topic refuses edits until unlocked.
    assert!(d.dispatch(&[s("board"), s("lock"), s("alice/notes/guide")], &owner).await.is_ok());
    let locked_edit = d
        .dispatch(&[s("board"), s("edit"), s("alice/notes/guide#1"), s("while locked")], &owner)
        .await;
    assert!(!locked_edit.is_ok(), "locked topic edit must be refused");
    assert!(d.dispatch(&[s("board"), s("unlock"), s("alice/notes/guide")], &owner).await.is_ok());

    // Curated board: non-moderator author cannot silently edit after publish; owner can.
    assert!(
        d.dispatch(&[s("board"), s("set"), s("alice/notes"), s("--policy"), s("curated")], &owner)
            .await
            .is_ok()
    );
    let curated_author_edit = d
        .dispatch(&[s("board"), s("edit"), s("alice/notes/guide#1"), s("sneak")], &writer)
        .await;
    assert!(!curated_author_edit.is_ok(), "non-moderator cannot directly edit curated board post");
    let curated_owner_edit = d
        .dispatch(&[s("board"), s("edit"), s("alice/notes/guide#1"), s("v4 owner")], &owner)
        .await;
    assert!(curated_owner_edit.is_ok(), "curated board owner edit should succeed: {curated_owner_edit:?}");

    // Locked board: only root can edit.
    assert!(
        d.dispatch(&[s("board"), s("set"), s("alice/notes"), s("--policy"), s("locked")], &owner)
            .await
            .is_ok()
    );
    let locked_board_owner = d
        .dispatch(&[s("board"), s("edit"), s("alice/notes/guide#1"), s("owner locked")], &owner)
        .await;
    assert!(!locked_board_owner.is_ok(), "locked board refuses non-root owner");
    let locked_board_root = d
        .dispatch(&[s("board"), s("edit"), s("alice/notes/guide#1"), s("v5 root")], &root)
        .await;
    assert!(locked_board_root.is_ok(), "root can edit on locked board: {locked_board_root:?}");
}

#[tokio::test]
async fn board_vi_sessions_edit_reply_post_and_cancel_cleanly() {
    let d = std::sync::Arc::new(test_dispatcher().await);
    d.kernel().broker().set_kj_dispatcher(&d).await;
    let (_root_id, root_ctx) = seed_root(&d);
    let alice_id = create_character(&d, "alice", false).await;
    let mut binding = worker_binding();
    binding.grant(Capability::Editor);
    let alice_ctx = register_bound_context(&d, "alice-ctx", alice_id, Some(root_ctx), &binding);
    let alice = caller_as(alice_id, Some(alice_ctx));

    assert!(d.dispatch(&[s("board"), s("create"), s("notes")], &alice).await.is_ok());
    assert!(
        d.dispatch(
            &[s("board"), s("post"), s("alice/notes"), s("--title"), s("Guide"), s("original line")],
            &alice,
        )
        .await
        .is_ok()
    );

    // 1. Edit existing post via --vi: typing does not mutate board before :w, :w commits, ZQ discards unsaved tail.
    let opened = d
        .dispatch(&[s("board"), s("edit"), s("alice/notes/guide#1"), s("--vi")], &alice)
        .await;
    let KjResult::Ok { data: Some(open_data), .. } = &opened else {
        panic!("expected ok-with-data from board edit --vi, got {opened:?}");
    };
    let sid = open_data["session"].as_u64().expect("session id");
    assert_eq!(open_data["text"].as_str(), Some("original line"));
    assert_eq!(open_data["path"].as_str(), Some("board://alice/notes/guide#1"));

    // Type without saving: live board post must stay unchanged.
    assert!(
        d.dispatch(&[s("vi"), s("keys"), sid.to_string(), s("iUPDATED <Esc>")], &alice)
            .await
            .is_ok()
    );
    let mid_read = d.dispatch(&[s("board"), s("read"), s("alice/notes/guide")], &alice).await;
    assert!(mid_read.message().contains("original line"), "{}", mid_read.message());
    assert!(!mid_read.message().contains("UPDATED"), "unsaved edit must not leak to board");

    // :w commits the edit.
    assert!(
        d.dispatch(&[s("vi"), s("keys"), sid.to_string(), s(":w<CR>")], &alice)
            .await
            .is_ok()
    );
    let saved_read = d.dispatch(&[s("board"), s("read"), s("alice/notes/guide")], &alice).await;
    assert!(saved_read.message().contains("UPDATED original line"), "{}", saved_read.message());

    // Further unsaved edit + ZQ discards and leaves saved text intact.
    assert!(
        d.dispatch(&[s("vi"), s("keys"), sid.to_string(), s("iNOPE <Esc>ZQ")], &alice)
            .await
            .is_ok()
    );
    let after_zq = d.dispatch(&[s("board"), s("read"), s("alice/notes/guide")], &alice).await;
    assert!(after_zq.message().contains("UPDATED original line"), "{}", after_zq.message());
    assert!(!after_zq.message().contains("NOPE"), "{}", after_zq.message());

    // 2. Reply via --vi: :w creates #2, editing more + :wq updates #2 without duplicating #3.
    let reply_open = d
        .dispatch(&[s("board"), s("reply"), s("alice/notes/guide"), s("--vi")], &alice)
        .await;
    let KjResult::Ok { data: Some(reply_data), .. } = &reply_open else {
        panic!("expected ok-with-data from board reply --vi, got {reply_open:?}");
    };
    let rsid = reply_data["session"].as_u64().expect("reply session id");
    assert!(
        d.dispatch(&[s("vi"), s("keys"), rsid.to_string(), s("ifirst draft<Esc>:w<CR>")], &alice)
            .await
            .is_ok()
    );
    assert!(
        d.dispatch(&[s("vi"), s("keys"), rsid.to_string(), s("A plus more<Esc>:wq<CR>")], &alice)
            .await
            .is_ok()
    );
    let after_reply = d.dispatch(&[s("board"), s("read"), s("alice/notes/guide")], &alice).await;
    let KjResult::Ok { data: Some(topic_data), message: reply_msg, .. } = &after_reply else {
        panic!("read failed: {after_reply:?}");
    };
    assert_eq!(topic_data["posts"].as_array().unwrap().len(), 2, "second :w must update #2, not create #3");
    assert!(reply_msg.contains("first draft plus more"), "{reply_msg}");

    // 3. Cancel reply via :q! posts nothing.
    let cancel_open = d
        .dispatch(&[s("board"), s("reply"), s("alice/notes/guide"), s("--vi")], &alice)
        .await;
    let KjResult::Ok { data: Some(cancel_data), .. } = &cancel_open else {
        panic!("expected ok-with-data, got {cancel_open:?}");
    };
    let csid = cancel_data["session"].as_u64().expect("cancel session id");
    assert!(
        d.dispatch(&[s("vi"), s("keys"), csid.to_string(), s("inever mind<Esc>:q!<CR>")], &alice)
            .await
            .is_ok()
    );
    let after_cancel = d.dispatch(&[s("board"), s("read"), s("alice/notes/guide")], &alice).await;
    let KjResult::Ok { data: Some(topic_data2), .. } = &after_cancel else {
        panic!("read failed: {after_cancel:?}");
    };
    assert_eq!(topic_data2["posts"].as_array().unwrap().len(), 2, "cancelled reply must not post");

    // 4. New topic via --vi: ZZ creates topic with #1.
    let post_open = d
        .dispatch(
            &[s("board"), s("post"), s("alice/notes"), s("--title"), s("Drafted in Vi"), s("--vi")],
            &alice,
        )
        .await;
    let KjResult::Ok { data: Some(post_data), .. } = &post_open else {
        panic!("expected ok-with-data from board post --vi, got {post_open:?}");
    };
    let psid = post_data["session"].as_u64().expect("post session id");
    assert!(
        d.dispatch(&[s("vi"), s("keys"), psid.to_string(), s("ihello from vi<Esc>ZZ")], &alice)
            .await
            .is_ok()
    );
    let new_topic = d
        .dispatch(&[s("board"), s("read"), s("alice/notes/drafted-in-vi")], &alice)
        .await;
    assert!(new_topic.is_ok(), "new topic should exist: {new_topic:?}");
    assert!(new_topic.message().contains("hello from vi"), "{}", new_topic.message());
}

#[tokio::test]
async fn board_list_all_and_empty_hint_and_compact_read_data() {
    let d = test_dispatcher().await;
    let (_root_id, root_ctx) = seed_root(&d);
    let alice_id = create_character(&d, "alice", false).await;
    let bob_id = create_character(&d, "bob", false).await;
    let alice_ctx = register_bound_context(&d, "alice-ctx", alice_id, Some(root_ctx), &worker_binding());
    let bob_ctx = register_bound_context(&d, "bob-ctx", bob_id, Some(root_ctx), &worker_binding());
    let alice = caller_as(alice_id, Some(alice_ctx));
    let bob = caller_as(bob_id, Some(bob_ctx));

    assert!(d.dispatch(&[s("board"), s("create"), s("notes")], &alice).await.is_ok());
    assert!(
        d.dispatch(&[s("board"), s("post"), s("alice/notes"), s("--title"), s("Hello"), s("body text")], &alice)
            .await
            .is_ok()
    );

    // Bob owns no boards: bare `kj board list` gives a helpful hint pointing to --all / recent.
    let bob_empty = d.dispatch(&[s("board"), s("list")], &bob).await;
    assert!(bob_empty.is_ok());
    assert!(bob_empty.message().contains("kj board list --all"), "{}", bob_empty.message());

    // `kj board list --all` discovers alice/notes from bob's context.
    let all = d.dispatch(&[s("board"), s("list"), s("--all")], &bob).await;
    assert!(all.is_ok());
    assert!(all.message().contains("alice/notes"), "{}", all.message());

    // `kj board read` keeps structured post metadata without duplicating full body text in `data`.
    let read = d.dispatch(&[s("board"), s("read"), s("alice/notes/hello")], &bob).await;
    let KjResult::Ok { message, data: Some(data), .. } = read else {
        panic!("expected ok-with-data");
    };
    assert!(message.contains("body text"), "{message}");
    let post0 = &data["posts"].as_array().unwrap()[0];
    assert_eq!(post0["number"].as_u64(), Some(1));
    assert!(post0.get("content").is_none(), "post content should not be duplicated in data: {post0}");

    // `kj board recent --others` excludes topics whose latest post is by the caller.
    let alice_others = d
        .dispatch(&[s("board"), s("recent"), s("--board"), s("alice/notes"), s("--others")], &alice)
        .await;
    assert!(alice_others.is_ok());
    assert!(alice_others.message().contains("(no recent topics)"), "{}", alice_others.message());
    let bob_others = d
        .dispatch(&[s("board"), s("recent"), s("--board"), s("alice/notes"), s("--others")], &bob)
        .await;
    assert!(bob_others.is_ok());
    assert!(bob_others.message().contains("Hello"), "{}", bob_others.message());
}

#[test]
fn notes_commands_accept_track_and_pagination() {
    let args = ["notes", "--track", "bass", "--performer", "alice", "--before", "7", "--limit", "4", "--json"];
    assert!(BoardArgs::try_parse_from(args).is_ok());
    assert!(BoardArgs::try_parse_from(["notes", "--topic", "alice/memory/vibe", "--before", "3"]).is_ok());
    assert!(BoardArgs::try_parse_from(["notes", "--topic", "alice/memory/vibe", "--track", "bass"]).is_err());
    assert!(BoardArgs::try_parse_from(["notes", "--topic", "alice/memory/vibe", "--performer", "alice"]).is_err());
    assert!(BoardArgs::try_parse_from(["notes", "--after", "1", "--before", "3"]).is_err());
    let args = ["add-note", "--track", "bass", "--at", "3", "--phrase-start", "0", "--score", "abc", "hello"];
    assert!(BoardArgs::try_parse_from(args).is_ok());
    for name in ["notes", "add-note"] {
        let mut command = BoardArgs::command();
        let help = command.find_subcommand_mut(name).unwrap().render_long_help().to_string();
        println!("{help}");
        assert!(help.contains("--track"));
    }
}

#[tokio::test]
async fn track_notes_share_posts_and_preserve_pagination_and_authors() {
    let d = test_dispatcher().await;
    let (_, root_ctx) = seed_root(&d);
    let musician = create_character(&d, "note-musician", false).await;
    let ctx = register_bound_context(&d, "notes-work", musician, Some(root_ctx), &worker_binding());
    let c = caller_as(musician, Some(ctx));
    d.kernel().arm_track_timeline(
        kaijutsu_types::TrackId::new("bass_1").unwrap(),
        kaijutsu_hyoushigi::TickClock::default(),
        kaijutsu_types::Tick::new(10),
    );
    for text in ["first", "second", "third"] {
        let result = d.dispatch(&[s("board"), s("add-note"), s("--track"), s("bass_1"), s("--at"), s("8"), s(text)], &c).await;
        assert!(result.is_ok(), "{}", result.message());
    }
    d.kernel().arm_track_timeline(
        kaijutsu_types::TrackId::new("bass-1").unwrap(),
        kaijutsu_hyoushigi::TickClock::default(),
        kaijutsu_types::Tick::new(10),
    );
    let other = d.dispatch(&[s("board"), s("add-note"), s("--track"), s("bass-1"), s("other track")], &c).await;
    assert!(other.is_ok(), "{}", other.message());
    let ordinary_read = d.dispatch(&[s("board"), s("read"), s("note-musician/music-notes/bass_1")], &c).await;
    assert!(ordinary_read.is_ok(), "{}", ordinary_read.message());
    assert!(!ordinary_read.message().contains("other track"));
    let root = caller_as(seed_root(&d).0, Some(root_ctx));
    let explicit = d.dispatch(&[s("board"), s("notes"), s("--track"), s("bass_1"), s("--performer"), musician.to_hex()], &root).await;
    let KjResult::Ok { data: Some(explicit_data), .. } = explicit else { panic!("notes failed: {explicit:?}") };
    assert_eq!(explicit_data["posts"].as_array().unwrap().len(), 3);
    let result = d.dispatch(&[s("board"), s("notes"), s("--track"), s("bass_1"), s("--before"), s("3"), s("--limit"), s("1"), s("--json")], &c).await;
    let KjResult::Ok { data: Some(data), .. } = result else { panic!("notes failed: {result:?}") };
    assert_eq!(data["posts"][0]["number"], 2);
    assert!(data["posts"][0]["body"].as_str().unwrap().contains("second"));
    assert_eq!(data["posts"][0]["author_id"], musician.to_hex());
    let result = d.dispatch(&[s("board"), s("notes"), s("--track"), s("bass_1"), s("--after"), s("1"), s("--limit"), s("1")], &c).await;
    let KjResult::Ok { data: Some(data), .. } = result else { panic!("notes failed: {result:?}") };
    assert_eq!(data["posts"].as_array().unwrap().len(), 1);
    assert_eq!(data["posts"][0]["number"], 2);
    let topic = d.dispatch(&[s("board"), s("notes"), s("--topic"), s("note-musician/music-notes/bass_1"), s("--after"), s("1"), s("--limit"), s("1")], &c).await;
    let KjResult::Ok { data: Some(topic_data), .. } = topic else { panic!("topic notes failed: {topic:?}") };
    assert_eq!(topic_data, data);
}

#[tokio::test]
async fn concurrent_first_track_notes_share_one_topic() {
    let d = test_dispatcher().await;
    let (_, root_ctx) = seed_root(&d);
    let musician = create_character(&d, "notes-race", false).await;
    let ctx = register_bound_context(&d, "notes-race-work", musician, Some(root_ctx), &worker_binding());
    let c = caller_as(musician, Some(ctx));
    d.kernel().arm_track_timeline(
        kaijutsu_types::TrackId::new("race").unwrap(),
        kaijutsu_hyoushigi::TickClock::default(),
        kaijutsu_types::Tick::new(3),
    );
    let first = [s("board"), s("add-note"), s("--track"), s("race"), s("first")];
    let second = [s("board"), s("add-note"), s("--track"), s("race"), s("second")];
    let (a, b) = tokio::join!(d.dispatch(&first, &c), d.dispatch(&second, &c));
    assert!(a.is_ok(), "{}", a.message());
    assert!(b.is_ok(), "{}", b.message());
    let result = d.dispatch(&[s("board"), s("notes"), s("--track"), s("race")], &c).await;
    let KjResult::Ok { data: Some(data), .. } = result else { panic!("notes failed: {result:?}") };
    assert_eq!(data["posts"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn track_notes_vi_save_updates_one_post_and_cancel_discards() {
    let d = Arc::new(test_dispatcher().await);
    d.kernel().broker().set_kj_dispatcher(&d).await;
    let (_, root_ctx) = seed_root(&d);
    let musician = create_character(&d, "notes-editor", false).await;
    let mut binding = worker_binding();
    binding.grant(Capability::Editor);
    let ctx = register_bound_context(&d, "notes-editor-work", musician, Some(root_ctx), &binding);
    let c = caller_as(musician, Some(ctx));
    d.kernel().arm_track_timeline(kaijutsu_types::TrackId::new("vi").unwrap(), kaijutsu_hyoushigi::TickClock::default(), kaijutsu_types::Tick::new(10));
    let opened = d.dispatch(&[s("board"), s("add-note"), s("--track"), s("vi"), s("--vi")], &c).await;
    let KjResult::Ok { data: Some(data), .. } = opened else { panic!("open failed: {opened:?}") };
    let sid = data["session"].as_u64().unwrap().to_string();
    assert!(d.dispatch(&[s("vi"), s("keys"), sid.clone(), s("ifirst<Esc>:w<CR>")], &c).await.is_ok());
    assert!(d.dispatch(&[s("vi"), s("keys"), sid, s("A more<Esc>:wq<CR>")], &c).await.is_ok());
    let opened = d.dispatch(&[s("board"), s("add-note"), s("--track"), s("vi"), s("--vi")], &c).await;
    let KjResult::Ok { data: Some(data), .. } = opened else { panic!("open failed: {opened:?}") };
    let sid = data["session"].as_u64().unwrap().to_string();
    assert!(d.dispatch(&[s("vi"), s("keys"), sid, s("idiscard<Esc>:q!<CR>")], &c).await.is_ok());
    let result = d.dispatch(&[s("board"), s("notes"), s("--track"), s("vi")], &c).await;
    let KjResult::Ok { data: Some(data), .. } = result else { panic!("notes failed: {result:?}") };
    assert_eq!(data["posts"].as_array().unwrap().len(), 1);
    let body = data["posts"][0]["body"].as_str().unwrap();
    assert!(body.contains("Opened at tick 10; sent at tick 10."), "{body}");
    assert!(body.contains("first more"), "{body}");
    assert!(!body.contains("discard"), "{body}");
}

#[tokio::test]
async fn track_board_ref_shares_score_context_board_and_keeps_character_refs() {
    let d = test_dispatcher().await;
    let (root, root_ctx) = seed_root(&d);
    let a = create_character(&d, "track-a", false).await;
    let b = create_character(&d, "track-b", false).await;
    let track_character = create_character(&d, "track", false).await;
    let score_ctx = register_bound_context(&d, "track-score", root, Some(root_ctx), &worker_binding());
    let a_ctx = register_bound_context(&d, "track-a-work", a, Some(root_ctx), &worker_binding());
    let b_ctx = register_bound_context(&d, "track-b-work", b, Some(root_ctx), &worker_binding());
    let track_ctx = register_bound_context(&d, "track-char-work", track_character, Some(root_ctx), &worker_binding());
    let a = caller_as(a, Some(a_ctx));
    let b = caller_as(b, Some(b_ctx));
    let track_caller = caller_as(track_character, Some(track_ctx));
    let root_caller = caller_as(root, Some(root_ctx));
    d.kernel_db().lock().upsert_track(&crate::kernel_db::PersistedTrack {
        track_id: "band".into(), period_ms: 500, beats_per_phrase: 4, playhead_tick: None,
        playing: false, score_context_id: Some(score_ctx), clock_kind: "system".into(),
    }).unwrap();
    let missing = d.dispatch(&[s("board"), s("topics"), s("@track/missing")], &a).await;
    assert!(!missing.is_ok(), "unknown tracks must fail");
    let posted = d.dispatch(&[s("board"), s("post"), s("@track/band"), s("--title"), s("handoff"), s("from a")], &a).await;
    assert!(posted.is_ok(), "{}", posted.message());
    let reply = d.dispatch(&[s("board"), s("reply"), s("@track/band/handoff"), s("from b")], &b).await;
    assert!(reply.is_ok(), "{}", reply.message());
    let read = d.dispatch(&[s("board"), s("read"), s("@track/band/handoff")], &a).await;
    assert!(read.message().contains("from a") && read.message().contains("from b"), "{}", read.message());
    let lock = d.dispatch(&[s("board"), s("lock"), s("@track/band/handoff")], &root_caller).await;
    assert!(lock.is_ok(), "{}", lock.message());
    assert!(!d.dispatch(&[s("board"), s("reply"), s("@track/band/handoff"), s("blocked")], &b).await.is_ok());
    assert!(d.dispatch(&[s("board"), s("create"), s("band")], &track_caller).await.is_ok());
    assert!(d.dispatch(&[s("board"), s("post"), s("track/band"), s("--title"), s("separate"), s("character")], &track_caller).await.is_ok());
    let separate = d.dispatch(&[s("board"), s("read"), s("track/band/separate")], &a).await;
    assert!(separate.is_ok(), "{}", separate.message());
}
