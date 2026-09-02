//! e2e: **an approval executes**. A human answering `kj ledger allow <id>`
//! on an ask that carries `exec_source` makes the kernel run that source —
//! in the ask's context, as the ask's principal, in the ask's cwd — and fill
//! the command/output block pair waiting on it. `docs/gate-shape-b.md`,
//! "Slice 5: approval executes".
//!
//! Everything here is driven through real surfaces on a live server:
//!
//!   * a pending ask with `exec_source` comes from calling the gated MCP
//!     `shell_write` tool over the wire, which refuses with `Pending` and
//!     leaves the durable row behind;
//!   * the answer comes from `kj ledger allow`/`deny` run through
//!     `shell_execute`, which is what a human at another seat does — and
//!     `kj ledger` refuses the seat that raised the ask, so every test here
//!     answers from a second context.
//!
//! The one thing not driven through a production surface is the block LINK.
//! No shipped path yet produces an ask that is BOTH executable and linked to
//! a pair: `shellExecute` links its pair but gates only through hooks
//! (`exec_source: None`), and `shell_write` carries the source but has no
//! pair at gate time. So the tests that need a linked pair author the pair
//! with the same block-store calls `execute_shell_command` uses and record
//! it with `KernelDb::link_ask_blocks` — the same call `execute_shell_command`
//! makes. That is the subscriber's input, and the subscriber is what these
//! tests cover.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{connect_client, run_local, start_server_with_kernel_handle};
use kaijutsu_client::{KernelHandle, RpcClient};
use kaijutsu_server::SharedKernel;
use kaijutsu_types::{BlockId, ContextId, PrincipalId, Status, ToolKind};

/// Poll until `check` returns true, or fail loudly. Every wait in this file
/// is on work a background driver does, so a timeout IS the bug.
async fn wait_for(label: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {label}");
}

/// A scratch directory that removes itself, so a test can prove a denied
/// command left nothing behind and a dead cwd is really dead.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kj-gate-exec-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn marker(&self) -> PathBuf {
        self.0.join("marker")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Two contexts on one connection: one that raises asks, one that answers
/// them. `kj ledger` refuses the seat that raised an ask, so a single
/// context cannot both ask and answer.
struct Seats {
    /// Held so the connection outlives the handle taken from it.
    _client: RpcClient,
    kj: KernelHandle,
    kernel: SharedKernel,
    worker: ContextId,
    approver: ContextId,
}

async fn seats() -> Seats {
    let (addr, kernel) = start_server_with_kernel_handle().await;
    let client = connect_client(addr).await;
    let (kj, _kernel_id) = client.bind_kernel().await.unwrap();
    let worker = kj.create_context("gate-exec-worker").await.unwrap();
    let approver = kj.create_context("gate-exec-approver").await.unwrap();
    Seats {
        _client: client,
        kj,
        kernel,
        worker,
        approver,
    }
}

impl Seats {
    /// Call the gated `shell_write` tool from the worker seat. It refuses
    /// with `Pending` and leaves one durable ask carrying `command` as its
    /// `exec_source`; this returns that ask's id.
    async fn raise(&self, command: &str) -> String {
        let kj = &self.kj;
        kj.join_context(self.worker, "gate-exec-worker").await.unwrap();
        let refusal = kj
            .call_mcp_tool("shell_write", &serde_json::json!({ "command": command }))
            .await;
        assert!(
            refusal.is_err(),
            "a gated shell_write must refuse rather than run: {refusal:?}"
        );
        let pending = self.kernel.kernel_db.lock().list_pending_asks().unwrap();
        let row = pending
            .into_iter()
            .find(|r| r.exec_source.as_deref() == Some(command.trim()))
            .unwrap_or_else(|| panic!("no pending ask carries {command:?} as its exec_source"));
        row.request_id
    }

    /// Answer from the approver seat, the way a human does.
    async fn answer(&self, request_id: &str, allow: bool) {
        let verb = if allow { "allow" } else { "deny" };
        let kj = &self.kj;
        kj.join_context(self.approver, "gate-exec-approver").await.unwrap();
        kj.shell_execute(
            &format!("kj ledger {verb} {request_id}"),
            self.approver,
            true,
        )
        .await
        .expect("shell_execute for the answer");
        // `shell_execute` returns as soon as the blocks exist; the command
        // itself runs in the background. Wait for the row to reach a
        // DECIDED status — not merely for it to leave the pending queue,
        // which `claim` alone already does — so a test that sequences
        // anything after an answer is sequencing after the decision.
        wait_for("the answer to land in the ledger", || self.decided(request_id)).await;
    }

    /// Has a human's answer been recorded on this ask yet?
    fn decided(&self, request_id: &str) -> bool {
        matches!(
            self.kernel.kernel_db.lock().get_approval(request_id).unwrap(),
            Some(row)
                if matches!(
                    row.status,
                    kaijutsu_kernel::ApprovalStatus::Allowed
                        | kaijutsu_kernel::ApprovalStatus::Denied
                )
        )
    }

    /// Author a command/output pair sitting `Waiting` on `request_id`, the
    /// shape `execute_shell_command` leaves behind when the gate refuses it.
    fn link_waiting_pair(&self, request_id: &str, code: &str) -> (BlockId, BlockId) {
        let documents = &self.kernel.documents;
        let after = documents.last_block_id(self.worker);
        let command_block_id = documents
            .insert_tool_call_as(
                self.worker,
                None,
                after.as_ref(),
                "shell",
                serde_json::json!({ "code": code }),
                Some(ToolKind::Shell),
                Some(PrincipalId::system()),
                None,
                None,
            )
            .unwrap();
        let output_block_id = documents
            .insert_tool_result_as(
                self.worker,
                &command_block_id,
                Some(&command_block_id),
                "",
                false,
                None,
                Some(ToolKind::Shell),
                Some(PrincipalId::system()),
                None,
            )
            .unwrap();
        documents
            .set_status(self.worker, &command_block_id, Status::Waiting)
            .unwrap();
        documents
            .set_status(self.worker, &output_block_id, Status::Waiting)
            .unwrap();
        self.kernel
            .kernel_db
            .lock()
            .link_ask_blocks(request_id, &command_block_id, &output_block_id)
            .expect("link the pair to the ask");
        (command_block_id, output_block_id)
    }

    fn block(&self, id: &BlockId) -> kaijutsu_types::BlockSnapshot {
        self.kernel
            .documents
            .get_block_snapshot(self.worker, id)
            .unwrap_or_else(|e| panic!("reading block {id}: {e}"))
            .unwrap_or_else(|| panic!("block {id} vanished"))
    }

    /// Is this answer still uncollected? Once redeemed it drops out.
    fn undelivered(&self, request_id: &str) -> bool {
        self.kernel
            .kernel_db
            .lock()
            .undelivered_answers()
            .unwrap()
            .iter()
            .any(|a| a.request_id == request_id)
    }

    fn worker_blocks(&self) -> Vec<kaijutsu_types::BlockSnapshot> {
        self.kernel.documents.block_snapshots(self.worker).unwrap()
    }
}

/// The headline. An allowed ask whose blocks are already waiting on it goes
/// `Waiting` → `Done` with the command's stdout in the output block, the
/// approval is spent exactly once, and a later ledger change does not run it
/// a second time.
///
/// Falsified by dropping the `redeem_ask` claim before the run (the second
/// ledger change would re-run it and the marker file's contents would
/// double), or by never executing (the pair stays `Waiting`).
#[test]
fn an_allowed_ask_fills_the_pair_that_was_waiting_on_it() {
    run_local(async {
        let scratch = Scratch::new("allowed");
        let s = seats().await;
        let marker = scratch.marker();
        let code = format!(
            "echo gate-executed >> {}\ncat {}",
            marker.display(),
            marker.display()
        );

        let ask = s.raise(&code).await;
        let (command_block_id, output_block_id) = s.link_waiting_pair(&ask, &code);
        assert_eq!(s.block(&output_block_id).status, Status::Waiting);

        s.answer(&ask, true).await;

        wait_for("the output block to settle", || {
            s.block(&output_block_id).status != Status::Waiting
        })
        .await;

        assert_eq!(
            s.block(&output_block_id).status,
            Status::Done,
            "an approved command that exited 0 must settle Done"
        );
        assert_eq!(
            s.block(&command_block_id).status,
            Status::Done,
            "the command block settles with its output block"
        );
        assert!(
            s.block(&output_block_id).content.contains("gate-executed"),
            "the output block must hold the command's stdout, got {:?}",
            s.block(&output_block_id).content
        );
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "gate-executed\n",
            "the approved command must have run exactly once"
        );
        assert!(
            !s.undelivered(&ask),
            "an executed ask must be redeemed, so its answer is no longer undelivered"
        );

        // A second ledger change must not run it again — exactly-once lives
        // in the redemption row, not in the driver's memory.
        s.kernel
            .kernel
            .ledger_flows()
            .publish(kaijutsu_kernel::flows::LedgerFlow::Changed { generation: 9_999 });
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "gate-executed\n",
            "a second ledger change must not re-run a redeemed ask"
        );
    });
}

/// An approved statement runs with the free-variable values the human read,
/// not with whatever the context holds by the time the answer lands: a
/// value moved after the ask is ignored, and a name that was unset then is
/// unset now even though it has since been given a value.
///
/// Falsified by skipping the seed: the marker reads `moved-appeared`.
#[test]
fn an_allowed_ask_runs_with_the_values_it_was_asked_about() {
    run_local(async {
        let scratch = Scratch::new("env");
        let s = seats().await;
        let marker = scratch.marker();
        s.kernel
            .kernel_db
            .lock()
            .set_context_env(s.worker, "GATE_FOO", "asked")
            .unwrap();
        let code = format!(
            "echo \"${{GATE_FOO}}-${{GATE_BAR}}\" >> {}",
            marker.display()
        );

        let ask = s.raise(&code).await;
        {
            let db = s.kernel.kernel_db.lock();
            db.set_context_env(s.worker, "GATE_FOO", "moved").unwrap();
            db.set_context_env(s.worker, "GATE_BAR", "appeared").unwrap();
        }
        let (_command_block_id, output_block_id) = s.link_waiting_pair(&ask, &code);

        s.answer(&ask, true).await;
        wait_for("the output block to settle", || {
            s.block(&output_block_id).status != Status::Waiting
        })
        .await;

        assert_eq!(
            s.block(&output_block_id).status,
            Status::Done,
            "stderr: {:?}",
            s.block(&output_block_id).stderr
        );
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "asked-\n",
            "the approved text must expand to the values the human read"
        );
    });
}

/// A denial settles the same pair to `Error` with the reason on stderr, and
/// runs nothing at all.
///
/// Falsified by executing on any answer rather than only on an allow: the
/// marker file would exist.
#[test]
fn a_denied_ask_settles_its_pair_to_error_and_runs_nothing() {
    run_local(async {
        let scratch = Scratch::new("denied");
        let s = seats().await;
        let marker = scratch.marker();
        let code = format!("echo should-not-run > {}", marker.display());

        let ask = s.raise(&code).await;
        let (command_block_id, output_block_id) = s.link_waiting_pair(&ask, &code);

        s.answer(&ask, false).await;

        wait_for("the output block to settle", || {
            s.block(&output_block_id).status != Status::Waiting
        })
        .await;

        assert_eq!(s.block(&output_block_id).status, Status::Error);
        assert_eq!(s.block(&command_block_id).status, Status::Error);
        let stderr = s.block(&output_block_id).stderr.unwrap_or_default();
        assert!(
            stderr.contains("denied by") && stderr.contains("nothing was run"),
            "the output block's stderr must carry the denial, got {stderr:?}"
        );
        assert!(
            !marker.exists(),
            "a denied command must not run: {} exists",
            marker.display()
        );
        assert!(
            !s.undelivered(&ask),
            "settling the blocks IS the delivery, so a denial with a pair is redeemed"
        );
    });
}

/// An allowed ask that names no blocks — the MCP `shell_write` shape, whose
/// tool_result was already settled when its turn ended. The driver authors a
/// fresh pair, runs into it, and seeds a block telling the model it already
/// ran and where the output is.
///
/// Falsified by keeping the old wake text: the seed would say "Nothing has
/// run yet. Try the same call again", which is now a lie and would make the
/// model run an approved action twice.
#[test]
fn an_allowed_ask_with_no_pair_authors_one_and_tells_the_model() {
    run_local(async {
        let scratch = Scratch::new("nopair");
        let s = seats().await;
        let marker = scratch.marker();
        let code = format!(
            "echo fresh-pair > {}\ncat {}",
            marker.display(),
            marker.display()
        );

        // The driver's own pair, found by the ToolCall that carries the
        // approved source — the woken turn writes tool blocks of its own
        // into this same context, so content alone would not identify it.
        let driver_pair = |s: &Seats| {
            let blocks = s.worker_blocks();
            let at = blocks
                .iter()
                .position(|b| {
                    b.kind == kaijutsu_types::BlockKind::ToolCall
                        && b.content.contains("echo fresh-pair")
                })?;
            blocks.get(at + 1).cloned()
        };

        let before = s.worker_blocks().len();
        let ask = s.raise(&code).await;
        s.answer(&ask, true).await;

        wait_for("the driver's own pair to settle", || {
            driver_pair(&s).is_some_and(|b| b.status == Status::Done)
        })
        .await;

        assert!(
            s.worker_blocks().len() > before,
            "the driver must author blocks into the ask's context"
        );
        let output = driver_pair(&s).expect("the fresh output block");
        assert_eq!(output.kind, kaijutsu_types::BlockKind::ToolResult);
        assert!(
            output.content.contains("fresh-pair"),
            "the fresh output block must hold the command's stdout, got {:?}",
            output.content
        );
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "fresh-pair\n");

        wait_for("the seed block naming the output", || {
            s.worker_blocks().iter().any(|b| {
                b.kind == kaijutsu_types::BlockKind::Text
                    && b.content.contains("It has already run")
                    && b.content.contains(&output.id.to_key())
            })
        })
        .await;

        let seed = s
            .worker_blocks()
            .into_iter()
            .find(|b| b.kind == kaijutsu_types::BlockKind::Text && b.content.contains("already run"))
            .expect("the seed block");
        assert!(
            !seed.content.contains("Nothing has run yet"),
            "the executed case must not tell the model to retry, got: {}",
            seed.content
        );
    });
}

/// The cwd on the ask is the directory the human was asked about. When it no
/// longer resolves, nothing runs and the pair says so by name.
///
/// Falsified by running anyway (in whatever directory the shell landed in):
/// the marker would be created somewhere and the pair would settle `Done`.
#[test]
fn an_allowed_ask_whose_cwd_is_gone_runs_nothing_and_names_the_directory() {
    run_local(async {
        let scratch = Scratch::new("deadcwd");
        let s = seats().await;
        let dir = scratch.path().to_path_buf();

        // The ask reads its cwd off the context's durable shell state, the
        // same row `cd` writes.
        s.kernel
            .kernel_db
            .lock()
            .upsert_context_shell(&kaijutsu_kernel::kernel_db::ContextShellRow {
                context_id: s.worker,
                cwd: Some(dir.to_string_lossy().into_owned()),
                updated_at: 0,
            })
            .unwrap();

        let code = "echo dead-cwd > ./marker".to_string();
        let ask = s.raise(&code).await;
        let (command_block_id, output_block_id) = s.link_waiting_pair(&ask, &code);
        assert_eq!(
            s.kernel
                .kernel_db
                .lock()
                .get_approval(&ask)
                .unwrap()
                .unwrap()
                .cwd
                .as_deref(),
            Some(dir.to_string_lossy().as_ref()),
            "the ask must have recorded the directory it was raised in"
        );

        std::fs::remove_dir_all(&dir).unwrap();
        s.answer(&ask, true).await;

        wait_for("the output block to settle", || {
            s.block(&output_block_id).status != Status::Waiting
        })
        .await;

        assert_eq!(s.block(&output_block_id).status, Status::Error);
        assert_eq!(s.block(&command_block_id).status, Status::Error);
        let stderr = s.block(&output_block_id).stderr.unwrap_or_default();
        assert!(
            stderr.contains(dir.to_string_lossy().as_ref()),
            "the refusal must name the directory that is gone, got {stderr:?}"
        );
        assert!(
            !scratch.marker().exists(),
            "nothing may run when the ask's directory is gone"
        );
    });
}

/// An archived context runs nothing, even when the answer was given while it
/// was still Live — the second of the two archived checks
/// (`docs/gate-shape-b.md`, "Archived contexts are inert"). `kj ledger allow`
/// refuses an archived context's ask, so the archive has to land AFTER the
/// answer, which is exactly the gap the second check exists for.
///
/// The driver is held inside a long-running approved command from an
/// unrelated context while the archive lands, so the answer under test is
/// read after it. Falsified by deleting the Live check the driver makes
/// before acting: the marker would appear.
#[test]
fn an_archived_context_runs_nothing_after_its_ask_is_answered() {
    run_local(async {
        let scratch = Scratch::new("archived");
        let s = seats().await;
        let marker = scratch.marker();

        // A third context whose approved command holds the single-threaded
        // driver busy. Its own seat cannot answer it, so the approver seat
        // does — same as every other answer here.
        let kj = &s.kj;
        let blocker_ctx = kj.create_context("gate-exec-blocker").await.unwrap();
        kj.join_context(blocker_ctx, "gate-exec-blocker").await.unwrap();
        let blocker_refusal = kj
            .call_mcp_tool("shell_write", &serde_json::json!({ "command": "/bin/sleep 5" }))
            .await;
        assert!(blocker_refusal.is_err(), "the blocker must be gated too");
        let blocker_ask = s
            .kernel
            .kernel_db
            .lock()
            .list_pending_asks()
            .unwrap()
            .into_iter()
            .find(|r| r.exec_source.as_deref() == Some("/bin/sleep 5"))
            .expect("the blocker ask")
            .request_id;

        let code = format!("echo archived-should-not-run > {}", marker.display());
        let ask = s.raise(&code).await;

        s.answer(&blocker_ask, true).await;
        // The driver redeems an ask before it runs it, so a decided answer
        // that is no longer undelivered means the run has started.
        wait_for("the blocker to start running", || {
            s.decided(&blocker_ask) && !s.undelivered(&blocker_ask)
        })
        .await;

        s.answer(&ask, true).await;
        assert!(
            s.kernel.kernel_db.lock().archive_context(s.worker).unwrap(),
            "the worker context was Live before this"
        );

        // Long enough for the blocker to finish and the driver to read the
        // next ledger change — the moment it would run this if the check
        // were missing.
        tokio::time::sleep(Duration::from_secs(8)).await;

        assert!(
            !marker.exists(),
            "an archived context must run nothing: {} exists",
            marker.display()
        );
        assert!(
            s.undelivered(&ask),
            "an answer nothing acted on stays uncollected rather than being spent"
        );
    });
}
