//! Live evaluation harness — drives a real model through the SSH+capnp stack.
//!
//! Skeleton + one fork. Spins up an isolated kernel rooted at
//! `target/live_eval/<ts>/`, points it at Claude Haiku via a generated
//! `models.toml`, runs one model turn, then hands off to a kaish harness
//! (`tests/live_eval/harness/*.kai`) that forks the conversation and asserts
//! on the resulting block topology with `kj block` JSON output.
//!
//! Env-gated on `KJ_LIVE_EVAL=1`. Requires `ANTHROPIC_API_KEY`. Each run
//! leaves its directory in place so failures can be inspected; cleanup is
//! manual (`rm -r target/live_eval/<ts>`).
//!
//! Re-run with:
//!   KJ_LIVE_EVAL=1 cargo test -p kaijutsu-server --test live_eval -- --nocapture

mod common;
use common::*;

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kaijutsu_client::KernelHandle;
use kaijutsu_types::{BlockKind, BlockQuery, ContextId, Role, Status};

const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";
const ANTHROPIC_PROVIDER: &str = "anthropic";

#[test]
fn live_eval_wc_clone_skeleton() {
    if std::env::var("KJ_LIVE_EVAL").is_err() {
        eprintln!(
            "skipping live_eval (set KJ_LIVE_EVAL=1 to run; \
             also requires ANTHROPIC_API_KEY)"
        );
        return;
    }
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        panic!("KJ_LIVE_EVAL=1 but ANTHROPIC_API_KEY is unset; cannot reach Claude");
    }

    // Persistent per-run dir under target/live_eval/<ts>/. Survives on
    // failure for inspection (kernel db, models.toml, workdir/).
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // workspace root = crate dir → ../.. (crates/<name> → workspace)
    let workspace_root = manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root above CARGO_MANIFEST_DIR")
        .to_path_buf();
    let run_root = workspace_root
        .join("target")
        .join("live_eval")
        .join(ts.to_string());
    let state_dir = run_root.join("state");
    let workdir = run_root.join("workdir");
    fs::create_dir_all(&state_dir).expect("create state dir");
    fs::create_dir_all(&workdir).expect("create workdir");
    eprintln!("[live_eval] run dir: {}", run_root.display());

    // Write models.toml so the kernel registers an anthropic provider with
    // Haiku as the default. API key resolves from ANTHROPIC_API_KEY env var.
    let models_toml = format!(
        r#"default_provider = "{ANTHROPIC_PROVIDER}"

[providers.{ANTHROPIC_PROVIDER}]
enabled = true
default_model = "{HAIKU_MODEL}"
api_key_env = "ANTHROPIC_API_KEY"

[model_aliases]
"#
    );
    fs::write(state_dir.join("models.toml"), models_toml).expect("write models.toml");

    // Concatenate the kaish harness sources into one program. No `source`
    // builtin in kaish today, so we splice them at submission time.
    let harness_dir = manifest.join("tests/live_eval/harness");
    let lib = fs::read_to_string(harness_dir.join("lib.kai"))
        .unwrap_or_else(|e| panic!("read lib.kai: {e}"));
    let kj = fs::read_to_string(harness_dir.join("kj.kai"))
        .unwrap_or_else(|e| panic!("read kj.kai: {e}"));
    let scenario = fs::read_to_string(harness_dir.join("wc_clone.kai"))
        .unwrap_or_else(|e| panic!("read wc_clone.kai: {e}"));
    let harness_program = format!("# === lib.kai ===\n{lib}\n# === kj.kai ===\n{kj}\n# === wc_clone.kai ===\n{scenario}\n");

    run_local(async move {
        let addr = start_server_with_state_dir(state_dir.clone()).await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.expect("bind_kernel");

        // Root context: where Haiku writes its first reply.
        let main = kernel.create_context("main").await.expect("create_context");
        kernel
            .join_context(main, "live_eval")
            .await
            .expect("join_context");
        kernel
            .set_context_model(main, ANTHROPIC_PROVIDER, HAIKU_MODEL)
            .await
            .expect("set_context_model");

        // One real turn through Haiku. Keep the prompt short to keep
        // the run cheap and the assistant block focused.
        let prompt = "In 3 short bullet points, outline the smallest plausible \
                      design for a `wc` (word/line/byte counter) clone in Rust. \
                      No code yet — just the design.";
        let _prompt_id = kernel
            .prompt(prompt, Some(HAIKU_MODEL), main)
            .await
            .expect("prompt");

        wait_for_assistant_block(&kernel, main, Duration::from_secs(120)).await;

        // Hand off to the kaish harness. It runs in `main` because `kj fork`
        // forks the executing context; its own tool blocks land here, so the
        // harness filters for Text-kind blocks when asserting on the
        // conversation.
        let (_cmd_id, output, status) = shell_exec_wait_timeout_inner(
            &kernel,
            &harness_program,
            main,
            120_000,
        )
        .await;

        eprintln!("\n=== harness output (run {ts}) ===\n{output}\n=== end ===\n");
        if !matches!(status, Status::Done) {
            panic!("harness shell run did not reach Status::Done (got {status:?})");
        }
        let fails: Vec<&str> = output
            .lines()
            .filter(|l| l.starts_with("not ok"))
            .collect();
        if !fails.is_empty() {
            panic!(
                "TAP failures ({} of them):\n{}\n\nrun dir preserved at: {}",
                fails.len(),
                fails.join("\n"),
                run_root.display()
            );
        }
        let oks: usize = output
            .lines()
            .filter(|l| l.starts_with("ok "))
            .count();
        if oks == 0 {
            panic!(
                "no TAP 'ok' lines emitted — harness probably didn't run.\
                 \nrun dir: {}",
                run_root.display()
            );
        }
        eprintln!("[live_eval] {oks} assertions passed; run dir at {}", run_root.display());
    });
}

/// Poll the context's blocks until a Done-status `Role::Model + BlockKind::Text`
/// block appears. Panics on timeout, surfacing the current block list.
async fn wait_for_assistant_block(kernel: &KernelHandle, ctx: ContextId, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() > deadline {
            let blocks = kernel
                .get_blocks(ctx, &BlockQuery::All)
                .await
                .unwrap_or_default();
            panic!(
                "wait_for_assistant_block: timeout after {}s; blocks so far: {:#?}",
                timeout.as_secs(),
                blocks
                    .iter()
                    .map(|b| (b.role.as_str(), b.kind.as_str(), b.status.as_str()))
                    .collect::<Vec<_>>()
            );
        }
        let blocks = kernel
            .get_blocks(ctx, &BlockQuery::All)
            .await
            .unwrap_or_default();
        let done = blocks.iter().any(|b| {
            b.role == Role::Model && b.kind == BlockKind::Text && b.status == Status::Done
        });
        if done {
            return;
        }
        // Surface early failure so we don't burn the full timeout.
        if let Some(err) = blocks.iter().find(|b| b.kind == BlockKind::Error) {
            panic!(
                "Error block appeared while waiting for assistant: {}",
                err.content
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Local copy of the e2e_kj_workflow `shell_exec_wait_timeout` helper.
/// Lifting the helper out of that file would be premature.
async fn shell_exec_wait_timeout_inner(
    kernel: &KernelHandle,
    code: &str,
    context_id: ContextId,
    timeout_ms: u64,
) -> (kaijutsu_types::BlockId, String, Status) {
    let cmd_block_id = kernel
        .shell_execute(code, context_id, false)
        .await
        .unwrap_or_else(|e| panic!("shell_execute failed: {e}"));
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if Instant::now() > deadline {
            let blocks = kernel
                .get_blocks(context_id, &BlockQuery::All)
                .await
                .unwrap_or_default();
            panic!(
                "harness shell timeout after {timeout_ms}ms\nblocks: {:#?}",
                blocks
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let blocks = kernel
            .get_blocks(context_id, &BlockQuery::All)
            .await
            .unwrap_or_default();
        if let Some(output) = blocks
            .iter()
            .find(|b| b.kind == BlockKind::ToolResult && b.tool_call_id == Some(cmd_block_id))
        {
            if matches!(output.status, Status::Done | Status::Error) {
                return (cmd_block_id, output.content.clone(), output.status);
            }
        }
    }
}
