//! e2e: the compose draft is a block, over a real SSH + Cap'n Proto round trip.
//!
//! The claim this file exists to pin is **zero-copy submit**. The old
//! `submit_input` read the draft, cleared it, and only *then* tried to author a
//! block — so a failure in between destroyed what someone had typed. When the
//! block you type into *is* the message you send, that interval cannot exist.
//! The assertion is therefore an identity one: the id that comes back from
//! submit is the id that held the draft. No injected fault is needed to prove a
//! window is gone if there is nothing to copy between.
//!
//! Nothing here decodes an operation. The draft is observed the same way any
//! other block is — through `getBlocks` and through the change feed — because
//! that is the point of making it a block.

mod common;
use common::{create_context};

use std::time::Duration;

use common::{
    connect_client, run_local, seed_turn_identity, start_server,
    start_server_with_mock_llm_kernel_handle,
};
use kaijutsu_client::{
    ContextMirror, FeedEvent, KernelHandle, RpcClient, context_feed_channel,
};
use kaijutsu_types::{BlockId, ContextId};
use kaijutsu_types::{BlockKind, BlockQuery, BlockSnapshot, ContentType, InputEdge, Role, Status};

/// Every block in the context, in document order.
async fn blocks(kernel: &KernelHandle, context_id: ContextId) -> Vec<BlockSnapshot> {
    kernel
        .get_blocks(context_id, &BlockQuery::All)
        .await
        .expect("block query")
}

/// The caller's draft block, if the kernel is holding one.
///
/// Looked up by status rather than by a remembered id on purpose: a test that
/// tracked the id itself could not catch a submit that authored a *second*
/// block and left the draft behind.
async fn draft(kernel: &KernelHandle, context_id: ContextId) -> Option<BlockSnapshot> {
    blocks(kernel, context_id)
        .await
        .into_iter()
        .find(|b| b.status == Status::Draft)
}

async fn open_context(kernel: &KernelHandle, label: &str) -> ContextId {
    let context_id = create_context(&kernel, label).await.unwrap();
    kernel.join_context(context_id, "draft-test").await.unwrap();
    context_id
}

async fn bind(client: &RpcClient) -> KernelHandle {
    client.bind_kernel().await.unwrap().0
}

/// Typing creates a draft block; it is `Draft`, `ephemeral`, and last.
#[test]
fn typing_creates_an_ephemeral_draft_block_at_the_end() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-create").await;

        kernel.edit_input(context_id, 0, "hello", 0).await.unwrap();

        let all = blocks(&kernel, context_id).await;
        let d = all.last().expect("a draft block exists");
        assert_eq!(d.status, Status::Draft, "the draft carries Status::Draft");
        assert!(d.ephemeral, "the draft is ephemeral");
        assert_eq!(d.kind, BlockKind::Text);
        assert_eq!(d.content, "hello");

        // And the read verb agrees with the block.
        let state = kernel.get_input_state(context_id).await.unwrap();
        assert_eq!(state.content, "hello");
    });
}

/// **The zero-copy claim.** Submit returns the id the draft already had.
#[test]
fn chat_submit_promotes_the_draft_rather_than_copying_it() {
    run_local(async {
        // Submit starts a model turn, which needs a performer and a distinct
        // reviewer; client creation leaves the performer unset.
        let (addr, live_kernel) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-submit").await;
        seed_turn_identity(&live_kernel, context_id);

        kernel
            .edit_input(context_id, 0, "what did we ship?", 0)
            .await
            .unwrap();
        let before = draft(&kernel, context_id)
            .await
            .expect("draft exists before submit");

        let result = kernel.submit_input(context_id, false).await.unwrap();

        assert_eq!(
            result.block_id, before.id,
            "submit must promote the draft in place, not author a copy"
        );

        let all = blocks(&kernel, context_id).await;
        let submitted = all
            .iter()
            .find(|b| b.id == before.id)
            .expect("the draft's block survives submit");
        assert_eq!(submitted.status, Status::Done);
        assert!(
            !submitted.ephemeral,
            "a submitted message is no longer ephemeral"
        );
        assert_eq!(submitted.content, "what did we ship?");
        assert!(
            draft(&kernel, context_id).await.is_none(),
            "no draft remains after submit"
        );
    });
}

/// **The player's edge.** `submitInput` can carry the newest block a client
/// had shown and how much of it, and the kernel stores both on the promoted
/// user block (docs/prompts.md, "The submit verb").
#[test]
fn chat_submit_stores_the_players_edge_on_the_promoted_block() {
    run_local(async {
        let (addr, live_kernel) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-edge").await;
        seed_turn_identity(&live_kernel, context_id);

        // An earlier block in the conversation — the edge the client claims
        // it had shown when the player hit Enter.
        kernel
            .edit_input(context_id, 0, "what did we ship?", 0)
            .await
            .unwrap();
        let earlier = kernel.submit_input(context_id, false).await.unwrap();

        kernel
            .edit_input(context_id, 0, "and what's next?", 0)
            .await
            .unwrap();
        let edge = kaijutsu_types::InputEdge {
            block: earlier.block_id,
            shown: Some(12),
        };
        let result = kernel
            .submit_input_with_edge(context_id, false, Some(edge))
            .await
            .unwrap();

        let all = blocks(&kernel, context_id).await;
        let submitted = all
            .iter()
            .find(|b| b.id == result.block_id)
            .expect("the second submit's block exists");
        assert_eq!(
            submitted.edge_block,
            Some(earlier.block_id),
            "the edge names the block the client had shown"
        );
        assert_eq!(
            submitted.edge_shown,
            Some(12),
            "the edge carries how much of that block was shown"
        );
    });
}

/// **The submit rc verb.** After the draft is promoted, the kernel fires
/// `submit` for the context's type with the submit facts as `KJ_*`
/// variables, awaited inline: whatever the script writes is durable in the
/// log, after the user block, before `submitInput` returns.
#[test]
fn chat_submit_fires_the_submit_verb_with_the_edge_facts() {
    run_local(async {
        use kaijutsu_kernel::VfsOps;

        let (addr, live_kernel) = start_server_with_mock_llm_kernel_handle().await;
        // The script is planted before the context exists; the lifecycle
        // reads the directory when it fires, not when the context is made.
        live_kernel
            .kernel
            .vfs()
            .write_all(
                std::path::Path::new("/config/rc/default/submit/S10-facts.kai"),
                b"set -e\nkj block create --role system --kind notification --content \"input=$KJ_INPUT_BLOCK edge=$KJ_EDGE_BLOCK shown=$KJ_EDGE_SHOWN tail=$KJ_LOG_TAIL live=$KJ_TURN_LIVE\"\n",
            )
            .await
            .expect("plant the submit script");
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-submit-verb").await;
        seed_turn_identity(&live_kernel, context_id);

        // Two settled blocks, inserted directly so no mock turn runs and
        // moves the tail: the player's edge is the older one, the log tail
        // is the newer one.
        let settled = |content: &str| {
            live_kernel
                .documents
                .insert_block(
                    context_id,
                    None,
                    None,
                    Role::Model,
                    BlockKind::Text,
                    content,
                    Status::Done,
                    ContentType::Plain,
                )
                .expect("insert a settled block")
        };
        let first = settled("first");
        let second = settled("second");

        kernel.edit_input(context_id, 0, "third", 0).await.unwrap();
        let edge = InputEdge { block: first, shown: Some(3) };
        let third = kernel
            .submit_input_with_edge(context_id, false, Some(edge))
            .await
            .unwrap()
            .block_id;

        // Read synchronously after the call returns: the script's block must
        // already be durable, and it must sit after the user block.
        let all = blocks(&kernel, context_id).await;
        let expected = format!(
            "input={} edge={} shown=3 tail={} live=",
            third.to_key(),
            first.to_key(),
            second.to_key(),
        );
        let facts = all
            .iter()
            .position(|b| b.kind == BlockKind::Notification && b.content.starts_with(&expected))
            .unwrap_or_else(|| {
                panic!(
                    "no notification starting with {expected:?}; notifications: {:?}",
                    all.iter()
                        .filter(|b| b.kind == BlockKind::Notification)
                        .map(|b| b.content.clone())
                        .collect::<Vec<_>>()
                )
            });
        let user = all
            .iter()
            .position(|b| b.id == third)
            .expect("the submitted block is in the log");
        assert!(facts > user, "the script's block lands after the user block");
        let live = all[facts].content.rsplit("live=").next().unwrap();
        assert_eq!(live, "false", "no turn was running when the submit arrived");
    });
}

/// The facts script, planted into the default type. Returns nothing; the
/// caller reads the notification it writes.
async fn plant_facts_script(live_kernel: &kaijutsu_server::SharedKernel) {
    use kaijutsu_kernel::VfsOps;
    live_kernel
        .kernel
        .vfs()
        .write_all(
            std::path::Path::new("/config/rc/default/submit/S10-facts.kai"),
            b"set -e\nkj block create --role system --kind notification --content \"input=$KJ_INPUT_BLOCK edge=$KJ_EDGE_BLOCK shown=$KJ_EDGE_SHOWN tail=$KJ_LOG_TAIL live=$KJ_TURN_LIVE\"\n",
        )
        .await
        .expect("plant the submit script");
}

/// The facts notification for `input`, or a panic listing what is there.
fn facts_for(all: &[BlockSnapshot], input: BlockId) -> String {
    let prefix = format!("input={} ", input.to_key());
    all.iter()
        .find(|b| b.kind == BlockKind::Notification && b.content.starts_with(&prefix))
        .map(|b| b.content.clone())
        .unwrap_or_else(|| {
            panic!(
                "no facts for {prefix:?}; notifications: {:?}",
                all.iter()
                    .filter(|b| b.kind == BlockKind::Notification)
                    .map(|b| b.content.clone())
                    .collect::<Vec<_>>()
            )
        })
}

/// `KJ_TURN_LIVE` reports the turn that was running when the submit
/// arrived, read before this submit's own turn is marked.
#[test]
fn chat_submit_reports_a_live_turn() {
    run_local(async {
        let (addr, live_kernel) = start_server_with_mock_llm_kernel_handle().await;
        plant_facts_script(&live_kernel).await;
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-submit-live").await;
        seed_turn_identity(&live_kernel, context_id);

        // The flag the server reads, set the way a running turn sets it.
        let _turn_lease = live_kernel.kernel.turns().begin(context_id);
        kernel.edit_input(context_id, 0, "while you work", 0).await.unwrap();
        let input = kernel.submit_input(context_id, false).await.unwrap().block_id;

        let all = blocks(&kernel, context_id).await;
        let facts = facts_for(&all, input);
        assert!(facts.ends_with("live=true"), "facts: {facts}");
    });
}

/// The log tail is the newest block the player could have seen, whatever
/// its ephemeral flag: an interrupt marker or a staging notice is shown to
/// the player, and a script comparing the edge to the tail must see the
/// same block on both sides. Only another player's draft is excluded.
#[test]
fn chat_submit_log_tail_counts_ephemeral_blocks_but_not_drafts() {
    run_local(async {
        let (addr, live_kernel) = start_server_with_mock_llm_kernel_handle().await;
        plant_facts_script(&live_kernel).await;
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-submit-ephemeral").await;
        seed_turn_identity(&live_kernel, context_id);

        let older = live_kernel
            .documents
            .insert_block(
                context_id,
                None,
                None,
                Role::Model,
                BlockKind::Text,
                "older",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        let marker = live_kernel
            .documents
            .insert_block(
                context_id,
                None,
                None,
                Role::System,
                BlockKind::Text,
                "Interrupted",
                Status::Done,
                ContentType::Plain,
            )
            .unwrap();
        live_kernel
            .documents
            .set_ephemeral(context_id, &marker, true)
            .unwrap();
        // Another player's unsent draft is never the tail.
        let other = kaijutsu_types::PrincipalId::new();
        live_kernel
            .documents
            .get_or_create_draft(context_id, other)
            .expect("another player's draft");

        kernel.edit_input(context_id, 0, "after the marker", 0).await.unwrap();
        let input = kernel
            .submit_input_with_edge(context_id, false, Some(InputEdge { block: marker, shown: None }))
            .await
            .unwrap()
            .block_id;

        let all = blocks(&kernel, context_id).await;
        let facts = facts_for(&all, input);
        assert!(
            facts.contains(&format!(" tail={} ", marker.to_key())),
            "the ephemeral marker is the tail, not {}: {facts}",
            older.to_key()
        );
    });
}

/// A failing submit script leaves an Error block and the submit succeeds:
/// the message is durable and the turn starts. Rendering is advisory.
#[test]
fn chat_submit_survives_a_failing_submit_script() {
    run_local(async {
        use kaijutsu_kernel::VfsOps;

        let (addr, live_kernel) = start_server_with_mock_llm_kernel_handle().await;
        live_kernel
            .kernel
            .vfs()
            .write_all(
                std::path::Path::new("/config/rc/default/submit/S10-broken.kai"),
                b"set -e\nexit 3\n",
            )
            .await
            .expect("plant the broken script");
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-submit-broken").await;
        seed_turn_identity(&live_kernel, context_id);

        kernel.edit_input(context_id, 0, "still sent", 0).await.unwrap();
        let input = kernel
            .submit_input(context_id, false)
            .await
            .expect("a script failure never refuses the submit")
            .block_id;

        let all = blocks(&kernel, context_id).await;
        let sent = all.iter().find(|b| b.id == input).expect("the message is durable");
        assert_eq!(sent.status, Status::Done);
        assert!(
            all.iter().any(|b| b.kind == BlockKind::Error && b.content.contains("S10-broken")),
            "the failure is an Error block naming the script: {:?}",
            all.iter().filter(|b| b.kind == BlockKind::Error).map(|b| b.content.clone()).collect::<Vec<_>>()
        );
    });
}

/// **The shipped example renders the edge.** `lib/submit/S10-edge.kai`,
/// linked into a type the way an operator opts in, turns the facts into one
/// notification naming the edge block's role, kind, first line, key, and
/// characters shown. Run against the real kernel, not a stub `kj`: it is the
/// kaish that reads `kj block read` and `jq` output that this pins.
#[test]
fn chat_submit_example_script_renders_the_edge_excerpt() {
    run_local(async {
        use kaijutsu_kernel::VfsOps;

        let (addr, live_kernel) = start_server_with_mock_llm_kernel_handle().await;
        let vfs = live_kernel.kernel.vfs();
        let body = vfs
            .read_all(std::path::Path::new("/config/rc/lib/submit/S10-edge.kai"))
            .await
            .expect("the shipped example is seeded");
        vfs.write_all(std::path::Path::new("/config/rc/default/submit/S10-edge.kai"), &body)
            .await
            .expect("link the example into the default type");
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-submit-example").await;
        seed_turn_identity(&live_kernel, context_id);

        let settled = |content: &str| {
            live_kernel
                .documents
                .insert_block(
                    context_id,
                    None,
                    None,
                    Role::Model,
                    BlockKind::Text,
                    content,
                    Status::Done,
                    ContentType::Plain,
                )
                .expect("insert a settled block")
        };
        let first = settled("The plan has three steps.\nSecond line never shows.");
        let _second = settled("second");

        kernel.edit_input(context_id, 0, "wait, which plan?", 0).await.unwrap();
        let edge = InputEdge { block: first, shown: Some(12) };
        kernel
            .submit_input_with_edge(context_id, false, Some(edge))
            .await
            .unwrap();

        let all = blocks(&kernel, context_id).await;
        let rendered = all
            .iter()
            .filter(|b| b.kind == BlockKind::Notification)
            .find(|b| b.content.starts_with("The player wrote the message above"))
            .unwrap_or_else(|| {
                panic!(
                    "no rendered edge; blocks: {:?}",
                    all.iter().map(|b| (b.kind, b.content.clone())).collect::<Vec<_>>()
                )
            });
        let expected = format!(
            "The player wrote the message above while looking at an earlier point in this context: model text \"The plan has three steps.\" (block {}, 12 characters shown). Blocks after it had not been read.",
            first.to_key()
        );
        assert_eq!(rendered.content, expected);
    });
}

/// The example stays silent when the player was looking at the log tail.
#[test]
fn chat_submit_example_script_is_silent_at_the_tail() {
    run_local(async {
        use kaijutsu_kernel::VfsOps;

        let (addr, live_kernel) = start_server_with_mock_llm_kernel_handle().await;
        let vfs = live_kernel.kernel.vfs();
        let body = vfs
            .read_all(std::path::Path::new("/config/rc/lib/submit/S10-edge.kai"))
            .await
            .expect("the shipped example is seeded");
        vfs.write_all(std::path::Path::new("/config/rc/default/submit/S10-edge.kai"), &body)
            .await
            .expect("link the example into the default type");
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-submit-tail").await;
        seed_turn_identity(&live_kernel, context_id);

        let tail = live_kernel
            .documents
            .insert_block(
                context_id,
                None,
                None,
                Role::Model,
                BlockKind::Text,
                "tail",
                Status::Done,
                ContentType::Plain,
            )
            .expect("insert a settled block");

        kernel.edit_input(context_id, 0, "ok", 0).await.unwrap();
        kernel
            .submit_input_with_edge(context_id, false, Some(InputEdge { block: tail, shown: None }))
            .await
            .unwrap();

        let all = blocks(&kernel, context_id).await;
        assert!(
            !all.iter().any(|b| b.content.starts_with("The player wrote the message above")),
            "an edge at the tail renders nothing"
        );
        assert!(
            !all.iter().any(|b| b.kind == BlockKind::Error),
            "the script exits cleanly: {:?}",
            all.iter().filter(|b| b.kind == BlockKind::Error).map(|b| b.content.clone()).collect::<Vec<_>>()
        );
    });
}

/// A whitespace-only draft is refused **without being cleared** — a stray Enter
/// neither sends nothing nor destroys what is there.
#[test]
fn an_empty_draft_is_refused_and_survives() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-empty").await;

        kernel.edit_input(context_id, 0, "   ", 0).await.unwrap();
        kernel
            .submit_input(context_id, false)
            .await
            .expect_err("an empty draft is refused");

        let d = draft(&kernel, context_id)
            .await
            .expect("the refused draft is still there");
        assert_eq!(d.content, "   ", "a refused submit destroys nothing");
    });
}

/// Shell mode cannot promote — a shell command is a `ToolCall` block, not the
/// user's `Text`. So the draft is *consumed*, and the order is the assertion:
/// the command block must exist before the draft is cleared.
#[test]
fn shell_submit_authors_the_command_before_clearing_the_draft() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-shell").await;

        kernel
            .edit_input(context_id, 0, "echo hi", 0)
            .await
            .unwrap();
        let result = kernel.submit_input(context_id, true).await.unwrap();

        let all = blocks(&kernel, context_id).await;
        let command = all
            .iter()
            .find(|b| b.id == result.block_id)
            .expect("the command block exists");
        assert_eq!(
            command.kind,
            BlockKind::ToolCall,
            "shell submit authors a ToolCall, not the draft"
        );
        assert!(
            draft(&kernel, context_id).await.is_none(),
            "the draft is consumed once the command block exists"
        );
    });
}

struct PauseShellSubmit {
    entered: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl kaijutsu_kernel::mcp::Hook for PauseShellSubmit {
    async fn invoke(
        &self,
        _params: &kaijutsu_kernel::mcp::KernelCallParams,
        _ctx: &kaijutsu_kernel::mcp::CallContext,
    ) -> kaijutsu_kernel::mcp::McpResult<()> {
        self.entered.notify_one();
        self.resume.notified().await;
        Ok(())
    }
}

async fn edit_during_shell_submit(replace: bool, restore_text: bool) {
    use kaijutsu_kernel::mcp::{GlobPattern, HookAction, HookBody, HookEntry, HookId};
    use std::sync::Arc;

    let (addr, live_kernel) = start_server_with_mock_llm_kernel_handle().await;
    let client = connect_client(addr).await;
    let kernel = bind(&client).await;
    let context_id = open_context(&kernel, "draft-shell-race").await;
    let pause = Arc::new(PauseShellSubmit {
        entered: tokio::sync::Notify::new(), resume: tokio::sync::Notify::new(),
    });
    live_kernel.kernel.broker().hooks().write().await.pre_call.entries.push(HookEntry {
        id: HookId("pause-shell-submit".into()),
        match_instance: None, match_tool: Some(GlobPattern("shell_write".into())),
        match_context: Some(context_id), match_principal: None,
        action: HookAction::Invoke(HookBody::Builtin { name: "pause".into(), hook: pause.clone() }),
        priority: 0, kaish_script_id: None,
    });
    kernel.edit_input(context_id, 0, "echo hi", 0).await.unwrap();
    let before = draft(&kernel, context_id).await.unwrap();
    let submit_kernel = kernel.clone();
    let submit = tokio::task::spawn_local(async move {
        submit_kernel.submit_input(context_id, true).await
    });
    tokio::time::timeout(Duration::from_secs(5), pause.entered.notified()).await
        .expect("submission reaches the hook after reading its draft");
    if replace {
        kernel.clear_input(context_id).await.unwrap();
        kernel.edit_input(context_id, 0, "echo hi", 0).await.unwrap();
    } else {
        kernel.edit_input(context_id, 7, " later", 0).await.unwrap();
        if restore_text {
            kernel.edit_input(context_id, 7, "", 6).await.unwrap();
        }
    }
    let edited = draft(&kernel, context_id).await.unwrap();
    assert_eq!(edited.id == before.id, !replace);
    pause.resume.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(5), submit).await.unwrap().unwrap().unwrap();
    let after = draft(&kernel, context_id).await.expect("newer typing must survive submission");
    assert_eq!(after.id, edited.id);
    assert_eq!(after.content, edited.content);
    let all = blocks(&kernel, context_id).await;
    let command = all.iter().find(|b| b.id == result.block_id).unwrap();
    let args: serde_json::Value = serde_json::from_str(command.tool_input.as_deref().unwrap()).unwrap();
    assert_eq!(args["code"], "echo hi", "the command uses the draft read before the await");
}

#[test]
fn shell_submit_preserves_edits_during_submission() {
    run_local(edit_during_shell_submit(false, false));
}

#[test]
fn shell_submit_preserves_a_replacement_draft_with_identical_text() {
    run_local(edit_during_shell_submit(true, false));
}

#[test]
fn shell_submit_preserves_edits_that_restore_the_original_text() {
    run_local(edit_during_shell_submit(false, true));
}

#[test]
fn refused_shell_submit_keeps_its_draft() {
    run_local(async {
        use kaijutsu_kernel::mcp::{GlobPattern, HookAction, HookEntry, HookId};

        let (addr, live_kernel) = start_server_with_mock_llm_kernel_handle().await;
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-shell-refused").await;
        live_kernel.kernel.broker().hooks().write().await.pre_call.entries.push(HookEntry {
            id: HookId("refuse-shell-submit".into()),
            match_instance: None, match_tool: Some(GlobPattern("shell_write".into())),
            match_context: Some(context_id), match_principal: None,
            action: HookAction::Deny("test refusal".into()), priority: 0, kaish_script_id: None,
        });
        kernel.edit_input(context_id, 0, "echo hi", 0).await.unwrap();
        let before = draft(&kernel, context_id).await.unwrap();
        let error = kernel.submit_input(context_id, true).await.unwrap_err();
        assert!(matches!(error, kaijutsu_client::RpcError::Refused(_)), "{error:?}");
        let after = draft(&kernel, context_id).await.expect("refusal preserves the draft");
        assert_eq!(after.id, before.id);
        assert_eq!(after.content, before.content);
    });
}

/// `clearInput` discards the caller's draft.
#[test]
fn clearing_removes_the_draft_block() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-clear").await;

        kernel.edit_input(context_id, 0, "nevermind", 0).await.unwrap();
        assert!(draft(&kernel, context_id).await.is_some());

        kernel.clear_input(context_id).await.unwrap();
        assert!(
            draft(&kernel, context_id).await.is_none(),
            "clear removes the draft block itself"
        );
        // Clearing a draft that is not there is not an error.
        kernel.clear_input(context_id).await.unwrap();
    });
}

/// Edits are **character**-indexed, not byte-indexed.
///
/// This is the coordinate doctrine the old input document never proved: callers
/// counted characters while its `InputDocEntry` type (deleted 2026-08-16)
/// bounds-checked Rust byte lengths. Editing after an emoji is where those two
/// disagree.
#[test]
fn draft_edits_are_character_indexed_through_multibyte_text() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-utf8").await;

        // 5 characters, 13 bytes.
        kernel.edit_input(context_id, 0, "日本語 🎵", 0).await.unwrap();
        // Append at character 5 — a byte-indexed kernel would panic or misplace.
        kernel.edit_input(context_id, 5, " ok", 0).await.unwrap();
        // Delete the two characters "語 " starting at character 2.
        kernel.edit_input(context_id, 2, "", 2).await.unwrap();

        let state = kernel.get_input_state(context_id).await.unwrap();
        assert_eq!(state.content, "日本🎵 ok");
    });
}

/// A co-player sees you typing: the draft rides the change feed like any other
/// block. A single shared input document could never do this.
#[test]
fn the_draft_rides_the_change_feed() {
    run_local(async {
        let addr = start_server().await;
        let client = connect_client(addr).await;
        let kernel = bind(&client).await;
        let context_id = open_context(&kernel, "draft-feed").await;

        // Subscribe FIRST, then snapshot — the mandated order.
        let (observer, mut rx) = context_feed_channel(256);
        kernel
            .subscribe_context(context_id, observer)
            .await
            .unwrap();
        let (snapshot, version) = kernel
            .get_blocks_versioned(context_id, &BlockQuery::All)
            .await
            .unwrap();
        let mut mirror = ContextMirror::new(context_id);
        mirror.apply_snapshot(snapshot, version).unwrap();

        kernel.edit_input(context_id, 0, "typing", 0).await.unwrap();
        kernel.edit_input(context_id, 6, " more", 0).await.unwrap();

        let target = kernel
            .get_blocks_versioned(context_id, &BlockQuery::All)
            .await
            .unwrap()
            .1;
        while mirror.version() < target {
            match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
                Ok(Some(FeedEvent::Changed(delivery))) => {
                    mirror.receive(delivery).expect("delivery applies cleanly")
                }
                Ok(Some(other)) => panic!("unexpected feed event: {other:?}"),
                Ok(None) => panic!("feed closed before version {target}"),
                Err(_) => panic!("timed out at {} waiting for {target}", mirror.version()),
            }
        }

        let seen = mirror
            .blocks()
            .iter()
            .find(|b| b.status == Status::Draft)
            .expect("the draft reached the mirror");
        assert_eq!(
            seen.content, "typing more",
            "a co-player follows the draft through the ordinary feed"
        );
    });
}
