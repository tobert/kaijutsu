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

/// Wait until at least `n_after` *new* Done assistant Text blocks have
/// appeared in `ctx`, then return the content of the latest one.
///
/// `prior_count` is the number of Done Model+Text blocks already present
/// before the prompt that this call is awaiting. The wait returns once
/// the count is strictly greater than `prior_count` and the newest such
/// block has reached `Status::Done`. This lets a multi-turn test
/// distinguish "the model already replied to an earlier prompt" from
/// "the prompt I just sent has been answered."
async fn wait_for_nth_assistant_text(
    kernel: &KernelHandle,
    ctx: ContextId,
    prior_count: usize,
    timeout: Duration,
) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() > deadline {
            let blocks = kernel
                .get_blocks(ctx, &BlockQuery::All)
                .await
                .unwrap_or_default();
            panic!(
                "wait_for_nth_assistant_text({prior_count}): timeout after {}s; \
                 blocks so far: {:#?}",
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
        // Surface error blocks early rather than burning the timeout.
        if let Some(err) = blocks.iter().find(|b| b.kind == BlockKind::Error) {
            panic!(
                "Error block appeared while waiting for assistant: {}",
                err.content
            );
        }
        let done_replies: Vec<&str> = blocks
            .iter()
            .filter(|b| {
                b.role == Role::Model
                    && b.kind == BlockKind::Text
                    && b.status == Status::Done
            })
            .map(|b| b.content.as_str())
            .collect();
        if done_replies.len() > prior_count {
            // Return the newest reply (last in committed order).
            return done_replies.last().copied().unwrap_or("").to_string();
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Count Done assistant Text blocks in a context — paired with
/// `wait_for_nth_assistant_text` so the caller can snapshot the count
/// *before* sending a new prompt.
async fn count_done_assistant_text(kernel: &KernelHandle, ctx: ContextId) -> usize {
    let blocks = kernel
        .get_blocks(ctx, &BlockQuery::All)
        .await
        .unwrap_or_default();
    blocks
        .iter()
        .filter(|b| {
            b.role == Role::Model && b.kind == BlockKind::Text && b.status == Status::Done
        })
        .count()
}

/// Tiny TAP-style accumulator for the Rust-driven scenarios. Each
/// assertion appends a `ok N - ...` or `not ok N - ...` line; the
/// failure list is what drives the final panic.
#[derive(Default)]
struct TapCounter {
    n: usize,
    lines: Vec<String>,
    failures: Vec<String>,
}

impl TapCounter {
    fn record(&mut self, ok: bool, msg: &str, detail: Option<String>) {
        self.n += 1;
        let line = if ok {
            format!("ok {} - {msg}", self.n)
        } else {
            let head = format!("not ok {} - {msg}", self.n);
            self.failures.push(head.clone());
            if let Some(d) = detail.as_ref() {
                self.lines.push(head.clone());
                format!("    detail: {d}")
            } else {
                head
            }
        };
        self.lines.push(line);
    }

    fn assert_contains_ci(&mut self, haystack: &str, needle: &str, msg: &str) {
        let ok = haystack.to_lowercase().contains(&needle.to_lowercase());
        let detail = if ok {
            None
        } else {
            Some(format!(
                "haystack ({} chars) does not contain '{needle}'; head: {:?}",
                haystack.len(),
                haystack.chars().take(160).collect::<String>(),
            ))
        };
        self.record(ok, msg, detail);
    }

    fn assert_not_contains_ci(&mut self, haystack: &str, needle: &str, msg: &str) {
        let ok = !haystack.to_lowercase().contains(&needle.to_lowercase());
        let detail = if ok {
            None
        } else {
            Some(format!(
                "haystack unexpectedly contained '{needle}'; head: {:?}",
                haystack.chars().take(160).collect::<String>(),
            ))
        };
        self.record(ok, msg, detail);
    }
}

/// Create the per-run state/work dirs under `target/live_eval/<suffix>-<ts>/`
/// and return the (run_root, state_dir) pair. The dir survives the test
/// for inspection; cleanup is manual.
fn setup_run_dirs(suffix: &str) -> (PathBuf, PathBuf) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root above CARGO_MANIFEST_DIR")
        .to_path_buf();
    let run_root = workspace_root
        .join("target")
        .join("live_eval")
        .join(format!("{suffix}-{ts}"));
    let state_dir = run_root.join("state");
    fs::create_dir_all(&state_dir).expect("create state dir");
    eprintln!("[live_eval] run dir: {}", run_root.display());
    (run_root, state_dir)
}

/// Write a `models.toml` that registers Anthropic with Haiku as default.
fn write_haiku_models_toml(state_dir: &std::path::Path) {
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

/// Conversation-as-session (Slice A) regression scenarios — drives real
/// Haiku turns through three behaviors that the unit tests can't reach:
///
/// 1. **Shell visible to next turn.** Mailbox catch_up picks up
///    user-initiated `kj shell` output between LLM turns, so the model
///    sees a sentinel printed via `echo` in turn 2's wire history.
/// 2. **Exclude on flushed block is a no-op.** Once a block has been
///    folded into the live session, `set_block_excluded` does not
///    remove it from the next wire payload (invariant #2 — the
///    Anthropic API has already seen prior turns; we can't unsay them).
/// 3. **Fork honors exclude.** Forking creates a fresh mailbox that
///    bootstraps from the durable block log via `translate_block`,
///    which *does* skip excluded blocks — so the forked conversation
///    no longer carries them.
///
/// All three scenarios share one server boot. Each costs ~1-2 Haiku
/// turns; the whole test is ~5-6 turns end-to-end (~$0.05, ~3-5 min
/// wallclock). Failures preserve the run dir under
/// `target/live_eval/slice_a-<ts>/` for inspection.
///
/// Run with:
///   KJ_LIVE_EVAL=1 cargo test -p kaijutsu-server --test live_eval \
///       live_eval_conversation_session_slice_a -- --nocapture
#[test]
fn live_eval_conversation_session_slice_a() {
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

    let (run_root, state_dir) = setup_run_dirs("slice_a");
    write_haiku_models_toml(&state_dir);

    let tap = std::sync::Arc::new(std::sync::Mutex::new(TapCounter::default()));
    let tap_for_async = tap.clone();
    let run_root_for_async = run_root.clone();

    run_local(async move {
        let addr = start_server_with_state_dir(state_dir.clone()).await;
        let client = connect_client(addr).await;
        let (kernel, _kernel_id) = client.bind_kernel().await.expect("bind_kernel");

        // ── Scenario 1: shell call between LLM turns is visible to turn 2 ──
        let ctx_shell = kernel
            .create_context("shell_visibility")
            .await
            .expect("create_context(shell_visibility)");
        kernel
            .join_context(ctx_shell, "live_eval")
            .await
            .expect("join_context(shell_visibility)");
        kernel
            .set_context_model(ctx_shell, ANTHROPIC_PROVIDER, HAIKU_MODEL)
            .await
            .expect("set_context_model(shell_visibility)");

        // Turn 1: warm-up — locks one user+assistant exchange into the mailbox.
        let prior = count_done_assistant_text(&kernel, ctx_shell).await;
        kernel
            .prompt(
                "Reply with just the single word READY and nothing else.",
                Some(HAIKU_MODEL),
                ctx_shell,
            )
            .await
            .expect("prompt 1 (shell)");
        let _r1 =
            wait_for_nth_assistant_text(&kernel, ctx_shell, prior, Duration::from_secs(60)).await;

        // Shell call between turns. user_initiated=true matches the path
        // the GUI takes when the human runs a kj-shell command; the kernel
        // auto-excludes both the command and result blocks (rpc.rs ~5331)
        // so the user can opt in by toggling visibility. We mimic that
        // opt-in by un-excluding both blocks before the second prompt —
        // the hydrate translator then folds the pair into a single user
        // message ("[User ran `echo ...`]\n<output>") on turn 2's catch_up.
        let sentinel = "ferris_marker_seven";
        let echo_cmd = format!("echo '{sentinel}'");
        let (cmd_block_id, result_block_id, sh_out, sh_status) = {
            let cmd_block_id = kernel
                .shell_execute(&echo_cmd, ctx_shell, true)
                .await
                .unwrap_or_else(|e| panic!("user shell_execute failed: {e}"));
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if Instant::now() > deadline {
                    panic!("user shell timeout");
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                let blocks = kernel
                    .get_blocks(ctx_shell, &BlockQuery::All)
                    .await
                    .unwrap_or_default();
                if let Some(output) = blocks.iter().find(|b| {
                    b.kind == BlockKind::ToolResult && b.tool_call_id == Some(cmd_block_id)
                }) {
                    if matches!(output.status, Status::Done | Status::Error) {
                        break (cmd_block_id, output.id, output.content.clone(), output.status);
                    }
                }
            }
        };
        assert!(
            matches!(sh_status, Status::Done),
            "shell exec failed: status={sh_status:?} out={sh_out}"
        );

        // Opt the shell pair into the conversation. The mailbox hasn't seen
        // these blocks yet (catch_up runs at the START of a prompt's
        // process_llm_stream), so flipping excluded now means turn 2's
        // catch_up will translate them with excluded=false. If catch_up had
        // already folded them in (excluded=true → skipped), this flip would
        // be a no-op under invariant #2 — same gotcha real users hit.
        kernel
            .set_block_excluded(ctx_shell, &cmd_block_id, false)
            .await
            .expect("unexclude shell cmd");
        kernel
            .set_block_excluded(ctx_shell, &result_block_id, false)
            .await
            .expect("unexclude shell result");

        // Turn 2: ask the model about the shell sentinel.
        let prior = count_done_assistant_text(&kernel, ctx_shell).await;
        kernel
            .prompt(
                "What single word did the most recent shell command print? \
                 Reply with only that word, lowercase, nothing else.",
                Some(HAIKU_MODEL),
                ctx_shell,
            )
            .await
            .expect("prompt 2 (shell)");
        let reply2 =
            wait_for_nth_assistant_text(&kernel, ctx_shell, prior, Duration::from_secs(60)).await;

        {
            let mut t = tap_for_async.lock().unwrap();
            let ok = reply2.to_lowercase().contains(&sentinel.to_lowercase());
            if !ok {
                // Dump the full block log for ctx_shell so we can see what
                // the model actually had access to. Helps tell "shell output
                // never landed in the block log" from "shell output landed
                // but hydrate didn't translate it into a user message."
                let blocks = kernel
                    .get_blocks(ctx_shell, &BlockQuery::All)
                    .await
                    .unwrap_or_default();
                eprintln!("=== shell-visibility block log ===");
                for b in &blocks {
                    eprintln!(
                        "  [{role:>6}/{kind:>10}/{status:>6}] {head}",
                        role = b.role.as_str(),
                        kind = b.kind.as_str(),
                        status = b.status.as_str(),
                        head = b.content.chars().take(120).collect::<String>(),
                    );
                }
                eprintln!("=== end block log ===");
            }
            t.assert_contains_ci(
                &reply2,
                sentinel,
                "shell output visible to next LLM turn (mailbox flush)",
            );
        }

        // ── Scenario 2: exclude on a flushed block is a no-op ──
        let ctx_excl = kernel
            .create_context("exclude_noop")
            .await
            .expect("create_context(exclude_noop)");
        kernel
            .join_context(ctx_excl, "live_eval")
            .await
            .expect("join_context(exclude_noop)");
        kernel
            .set_context_model(ctx_excl, ANTHROPIC_PROVIDER, HAIKU_MODEL)
            .await
            .expect("set_context_model(exclude_noop)");

        let topic = "pineapples";
        let prior = count_done_assistant_text(&kernel, ctx_excl).await;
        kernel
            .prompt(
                &format!(
                    "Remember that I am asking about {topic}. Just acknowledge with the \
                     single word ACK."
                ),
                Some(HAIKU_MODEL),
                ctx_excl,
            )
            .await
            .expect("prompt 1 (exclude)");
        wait_for_nth_assistant_text(&kernel, ctx_excl, prior, Duration::from_secs(60)).await;

        // Exclude the user prompt block AFTER it has been folded into the mailbox.
        let blocks = kernel
            .get_blocks(ctx_excl, &BlockQuery::All)
            .await
            .expect("blocks (exclude scenario)");
        let user_block = blocks
            .iter()
            .find(|b| b.role == Role::User && b.kind == BlockKind::Text)
            .expect("user prompt Text block exists in exclude_noop");
        kernel
            .set_block_excluded(ctx_excl, &user_block.id, true)
            .await
            .expect("set_block_excluded");

        // Turn 2: the model should STILL recall the topic — the live session
        // was committed at turn 1, exclude on a committed block is a no-op.
        let prior = count_done_assistant_text(&kernel, ctx_excl).await;
        kernel
            .prompt(
                "What single topic did I ask about earlier? Reply with one lowercase \
                 word, no punctuation, nothing else.",
                Some(HAIKU_MODEL),
                ctx_excl,
            )
            .await
            .expect("prompt 2 (exclude)");
        let recall =
            wait_for_nth_assistant_text(&kernel, ctx_excl, prior, Duration::from_secs(60)).await;

        {
            let mut t = tap_for_async.lock().unwrap();
            t.assert_contains_ci(
                &recall,
                topic,
                "excluded block stays visible to live session (invariant #2)",
            );
        }

        // ── Scenario 3: fork is a boundary event; exclude takes effect after fork ──
        // Use a fresh context (not ctx_excl) so the model's prior reply
        // mentioning the topic doesn't leak through the fork. The block we
        // exclude is the *second* user turn — leaves the first user/assistant
        // pair intact so the forked wire history starts with a user message
        // (Anthropic requires user-first alternation).
        let ctx_fork = kernel
            .create_context("fork_pre_exclude")
            .await
            .expect("create_context(fork_pre_exclude)");
        kernel
            .join_context(ctx_fork, "live_eval")
            .await
            .expect("join_context(fork_pre_exclude)");
        kernel
            .set_context_model(ctx_fork, ANTHROPIC_PROVIDER, HAIKU_MODEL)
            .await
            .expect("set_context_model(fork_pre_exclude)");

        let fork_topic = "bananas";

        // Turn 1: greeting — leaves an unambiguous user-message head on the
        // wire so the fork's bootstrap can't produce an assistant-leading
        // conversation when the secret turn is excluded.
        let prior = count_done_assistant_text(&kernel, ctx_fork).await;
        kernel
            .prompt(
                "Reply with exactly the word HELLO and nothing else.",
                Some(HAIKU_MODEL),
                ctx_fork,
            )
            .await
            .expect("prompt 1 (fork scenario)");
        wait_for_nth_assistant_text(&kernel, ctx_fork, prior, Duration::from_secs(60)).await;

        // Turn 2: the secret turn — this user block is the one we exclude.
        // The assistant reply is just "ACK" so it doesn't echo the topic.
        let prior = count_done_assistant_text(&kernel, ctx_fork).await;
        kernel
            .prompt(
                &format!(
                    "I am asking about {fork_topic}. Reply with the single word ACK \
                     and nothing else — do NOT repeat the topic."
                ),
                Some(HAIKU_MODEL),
                ctx_fork,
            )
            .await
            .expect("prompt 2 (fork scenario)");
        wait_for_nth_assistant_text(&kernel, ctx_fork, prior, Duration::from_secs(60)).await;

        // Exclude the second user Text block (the one carrying the topic).
        let blocks = kernel
            .get_blocks(ctx_fork, &BlockQuery::All)
            .await
            .expect("blocks (fork scenario)");
        let secret_user_block = blocks
            .iter()
            .filter(|b| b.role == Role::User && b.kind == BlockKind::Text)
            .nth(1)
            .expect("two user prompt blocks present in ctx_fork");
        // Defence-in-depth: confirm the block we're excluding really
        // contains the topic so future prompt rewording can't silently
        // make scenario 3 vacuous.
        assert!(
            secret_user_block.content.to_lowercase().contains(fork_topic),
            "expected to exclude the user block containing '{fork_topic}', \
             got content head: {:?}",
            secret_user_block.content.chars().take(120).collect::<String>(),
        );
        kernel
            .set_block_excluded(ctx_fork, &secret_user_block.id, true)
            .await
            .expect("set_block_excluded (fork scenario)");

        // Fork the context via user-initiated `kj fork`. The new mailbox
        // bootstraps from the durable block log, where the excluded user
        // block is skipped at translate-time.
        let fork_label = "fork_after_exclude";
        let (fork_block, fork_out, fork_status) = {
            let cmd_block_id = kernel
                .shell_execute(&format!("kj fork --name {fork_label}"), ctx_fork, true)
                .await
                .unwrap_or_else(|e| panic!("fork shell_execute failed: {e}"));
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if Instant::now() > deadline {
                    panic!("kj fork timeout");
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                let blocks = kernel
                    .get_blocks(ctx_fork, &BlockQuery::All)
                    .await
                    .unwrap_or_default();
                if let Some(output) = blocks.iter().find(|b| {
                    b.kind == BlockKind::ToolResult && b.tool_call_id == Some(cmd_block_id)
                }) {
                    if matches!(output.status, Status::Done | Status::Error) {
                        break (cmd_block_id, output.content.clone(), output.status);
                    }
                }
            }
        };
        let _ = fork_block;
        assert!(
            matches!(fork_status, Status::Done),
            "kj fork failed: status={fork_status:?} out={fork_out}"
        );

        let ctxs = kernel.list_contexts().await.expect("list_contexts");
        let forked = ctxs
            .iter()
            .find(|c| c.label == fork_label)
            .unwrap_or_else(|| {
                panic!(
                    "forked context '{fork_label}' not found; labels seen: {:?}",
                    ctxs.iter().map(|c| c.label.as_str()).collect::<Vec<_>>()
                )
            })
            .id;
        // Fork must produce a new ContextId — that's what triggers a
        // fresh mailbox in process_llm_stream's cache_lock lookup.
        assert_ne!(
            forked, ctx_fork,
            "fork produced a new ContextId (mailbox boundary)"
        );

        // Turn in the forked context. The model should NOT recall the topic
        // because the only block that mentioned it was excluded, and
        // translate_block skips excluded blocks during bootstrap.
        let prior = count_done_assistant_text(&kernel, forked).await;
        kernel
            .prompt(
                "What topic did the user ask about earlier in this conversation? \
                 Reply with the single most relevant lowercase word, or the word \
                 'none' if no topic was raised. No punctuation, nothing else.",
                Some(HAIKU_MODEL),
                forked,
            )
            .await
            .expect("prompt (fork recall)");
        let post_fork =
            wait_for_nth_assistant_text(&kernel, forked, prior, Duration::from_secs(60)).await;

        {
            let mut t = tap_for_async.lock().unwrap();
            t.assert_not_contains_ci(
                &post_fork,
                fork_topic,
                "fork re-hydrate drops excluded block (boundary event)",
            );
        }
    });

    let final_tap = tap.lock().unwrap();
    eprintln!(
        "\n=== slice_a TAP output ({} assertions; run dir {}) ===",
        final_tap.n,
        run_root_for_async.display()
    );
    for line in &final_tap.lines {
        eprintln!("{line}");
    }
    eprintln!("=== end ===\n");

    if !final_tap.failures.is_empty() {
        panic!(
            "{} TAP failure(s):\n{}\n\nrun dir preserved at: {}",
            final_tap.failures.len(),
            final_tap.failures.join("\n"),
            run_root_for_async.display()
        );
    }
}
