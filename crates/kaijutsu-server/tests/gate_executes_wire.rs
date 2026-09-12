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
//!     `shell_execute`, which is what the assigned reviewer does. The worker
//!     and reviewer use distinct authenticated credentials; eligibility comes
//!     from the ask's snapped actor and reviewer IDs, not their contexts.
//!
//! Two origins produce an executable ask, and both are driven here:
//!
//!   * `shell_write` carries the source but has no pair at gate time, so
//!     the subscriber-focused tests author the pair with the same
//!     block-store calls `execute_shell_command` uses and record it with
//!     `KernelDb::link_ask_blocks` — the same call `execute_shell_command`
//!     makes. That isolates the subscriber from the gate.
//!   * `shellExecute` — the shell box — authors its pair BEFORE gating, and
//!     a PreCall `Ask` hook on `shell_write` refuses it with the command as
//!     `exec_source` and the pair linked to the ask. The `shell_box_*` tests
//!     drive that whole shipped path: the pair the human watched go
//!     `Waiting` is the one that fills.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::run_local;
use kaijutsu_client::{KernelHandle, KeySource, RpcClient, RpcError, SshClient, SshConfig};
use kaijutsu_kernel::mcp::{AskSpec, GlobPattern, HookAction, HookEntry, HookId};
use kaijutsu_kernel::PairOwner;
use kaijutsu_server::{AuthDb, SharedKernel, SshServer, SshServerConfig};
use kaijutsu_types::{BlockId, BlockKind, ContextId, PrincipalId, Status, ToolKind};
use russh::keys::{Algorithm, PrivateKey};

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

/// A worker actor and its assigned reviewer on distinct authenticated
/// connections. They use separate contexts for the wire calls, but approval
/// eligibility is the ask's snapped actor/reviewer identity, independent of
/// which context the reviewer uses.
struct Seats {
    /// Held so the connection outlives the handle taken from it.
    _worker_client: RpcClient,
    _approver_client: RpcClient,
    worker_kj: KernelHandle,
    approver_kj: KernelHandle,
    kernel: SharedKernel,
    worker: ContextId,
    approver: ContextId,
}

async fn seats() -> Seats {
    let tmp = tempfile::tempdir().unwrap();
    let auth_db_path = tmp.path().join("auth.db");
    let worker_principal = PrincipalId::new();
    let approver_principal = PrincipalId::new();
    let worker_key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
    let approver_key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519).unwrap();
    let auth_db = AuthDb::open(&auth_db_path).unwrap();
    auth_db.add_key(worker_principal, worker_key.public_key(), Some("gate-worker")).unwrap();
    auth_db.add_key(approver_principal, approver_key.public_key(), Some("gate-approver")).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = SshServerConfig::ephemeral(addr.port());
    config.auth_db_path = Some(auth_db_path);
    let (kernel_tx, kernel_rx) = tokio::sync::oneshot::channel();
    tokio::task::spawn_local(async move {
        SshServer::new(config).run_on_listener_with_kernel_sink(listener, kernel_tx).await.unwrap();
    });
    let kernel = kernel_rx.await.unwrap();
    {
        let db = kernel.kernel_db.lock();
        db.insert_character(&kaijutsu_kernel::kernel_db::CharacterRow {
            principal_id: worker_principal,
            name: "gate-worker".into(),
            created_at: kaijutsu_types::now_millis() as i64,
            retired_at: None,
            handoff_ctx: None,
        }).unwrap();
        db.insert_character(&kaijutsu_kernel::kernel_db::CharacterRow {
            principal_id: approver_principal,
            name: "gate-approver".into(),
            created_at: kaijutsu_types::now_millis() as i64,
            retired_at: None,
            handoff_ctx: None,
        }).unwrap();
    }
    let connect = |key: PrivateKey| async move {
        let config = SshConfig { host: addr.ip().to_string(), port: addr.port(), username: "gate".into(), key_source: KeySource::InMemory(Arc::new(key)), insecure: true };
        let mut ssh = SshClient::new(config);
        RpcClient::new(ssh.connect().await.unwrap().into_stream()).await.unwrap()
    };
    let worker_client = connect(worker_key).await;
    let approver_client = connect(approver_key).await;
    let (worker_kj, _) = worker_client.bind_kernel().await.unwrap();
    let (approver_kj, _) = approver_client.bind_kernel().await.unwrap();
    let worker = worker_kj.create_context("gate-exec-worker").await.unwrap();
    let approver = approver_kj.create_context("gate-exec-approver").await.unwrap();
    kernel
        .kernel_db
        .lock()
        .update_context_review(worker, Some(worker_principal), Some(approver_principal))
        .unwrap();
    Seats {
        _worker_client: worker_client,
        _approver_client: approver_client,
        worker_kj,
        approver_kj,
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
        let kj = &self.worker_kj;
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
        let kj = &self.approver_kj;
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
    /// shape `execute_shell_command` (`owner: PairOwner::Session`) or a
    /// model's own tool call (`owner: PairOwner::Turn`) leaves behind when
    /// the gate refuses it.
    fn link_waiting_pair(&self, request_id: &str, code: &str, owner: PairOwner) -> (BlockId, BlockId) {
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
            .link_ask_blocks(request_id, &command_block_id, &output_block_id, owner)
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

    /// Make every shell submission from the worker seat ask a human, the
    /// way the lfm2d hook does for a seat that scores its shell. Scoped to
    /// the worker context so the approver's `kj ledger` answers, which
    /// take the same `shell_execute` path, are not gated by it.
    async fn install_ask_hook_on_worker(&self) {
        self.kernel
            .kernel
            .broker()
            .hooks()
            .write()
            .await
            .pre_call
            .entries
            .push(HookEntry {
                id: HookId("wire-ask-worker".into()),
                match_instance: None,
                match_tool: Some(GlobPattern("shell_write".into())),
                match_context: Some(self.worker),
                match_principal: None,
                action: HookAction::Ask(AskSpec {
                    description: Some("the wire test wants a human".into()),
                }),
                priority: 0,
                kaish_script_id: None,
            });
    }

    /// Submit `code` from the shell box — `shell_execute`, the path a human
    /// typing at the interactive shell takes. With the ask hook installed
    /// it refuses `Pending`; this returns that ask's id and the pair the
    /// refusal left `Waiting` in the worker context.
    async fn submit_from_shell_box(&self, code: &str) -> (String, BlockId, BlockId) {
        let kj = &self.worker_kj;
        kj.join_context(self.worker, "gate-exec-worker").await.unwrap();
        let refusal = match kj.shell_execute(code, self.worker, true).await {
            Err(RpcError::Refused(refusal)) => refusal,
            other => panic!("a gated shell_execute must refuse with a Refusal: {other:?}"),
        };
        assert!(refusal.is_pending(), "the refusal must be a pending ask: {refusal:?}");
        let ask = refusal.ask_id().expect("a pending refusal names its ask").to_string();

        // The ToolCall block renders its input as JSON, so a multi-line
        // command carries an escaped newline there; the first line is a
        // safe substring of it.
        let first_line = code.trim().lines().next().unwrap_or_default();
        let blocks = self.worker_blocks();
        let at = blocks
            .iter()
            .position(|b| {
                b.kind == BlockKind::ToolCall
                    && b.status == Status::Waiting
                    && b.content.contains(first_line)
            })
            .expect("the refused command's ToolCall block, left Waiting");
        let command_block_id = blocks[at].id.clone();
        let output = blocks
            .get(at + 1)
            .filter(|b| b.kind == BlockKind::ToolResult)
            .expect("the ToolResult block right after the refused command");
        assert_eq!(output.status, Status::Waiting, "a pending ask leaves its pair Waiting");
        (ask, command_block_id, output.id.clone())
    }
}

/// The headline. An allowed ask whose blocks are already waiting on it goes
/// `Waiting` → `Done` with the command's stdout in the output block, the
/// approval is spent exactly once, and a later ledger change does not run it
/// a second time. The pair is `PairOwner::Session` — a connected session's
/// own blocks — so nobody gets told.
///
/// Falsified by dropping the `redeem_ask` claim before the run (the second
/// ledger change would re-run it and the marker file's contents would
/// double), by never executing (the pair stays `Waiting`), or by telling a
/// session-owned pair's context anyway (a seed block would appear).
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
        let (command_block_id, output_block_id) = s.link_waiting_pair(&ask, &code, PairOwner::Session);
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

        // Give a seed time to appear if the driver were going to write one,
        // then insist it did not: a session-owned pair tells nobody.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !s.worker_blocks().iter().any(|b| b.content.contains("It has run.")),
            "a session-owned pair must not get a seed telling anyone it ran"
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

/// A model's own linked pair (`PairOwner::Turn`) gets the same seed a
/// driver-authored pair does: its turn ended at the gate too, and the fill
/// is an in-place edit its cached mailbox will not re-read on its own
/// (`docs/gate-shape-b.md`, "The subscriber, in order").
///
/// Falsified by treating every linked pair as told-nobody regardless of
/// owner: no "It has run." seed would appear and a delegated turn would
/// never learn its approved tool call ran.
#[test]
fn an_allowed_ask_that_fills_a_turns_pair_tells_the_model() {
    run_local(async {
        let scratch = Scratch::new("turnpair");
        let s = seats().await;
        let marker = scratch.marker();
        let code = format!(
            "echo turn-pair-ran >> {}\ncat {}",
            marker.display(),
            marker.display()
        );

        let ask = s.raise(&code).await;
        let (command_block_id, output_block_id) =
            s.link_waiting_pair(&ask, &code, PairOwner::Turn);

        s.answer(&ask, true).await;

        wait_for("the output block to settle", || {
            s.block(&output_block_id).status != Status::Waiting
        })
        .await;

        assert_eq!(s.block(&output_block_id).status, Status::Done);
        assert_eq!(s.block(&command_block_id).status, Status::Done);
        assert!(
            s.block(&output_block_id).content.contains("turn-pair-ran"),
            "the output block must hold the command's stdout, got {:?}",
            s.block(&output_block_id).content
        );
        assert!(!s.undelivered(&ask), "an executed ask must be redeemed");

        wait_for("the seed block saying it ran", || {
            s.worker_blocks().iter().any(|b| {
                b.kind == kaijutsu_types::BlockKind::Text
                    && b.content.contains("approved the action")
                    && b.content.contains("It has run.")
            })
        })
        .await;

        let blocks = s.worker_blocks();
        let output_index = blocks
            .iter()
            .position(|b| b.id == output_block_id)
            .expect("the filled output block");
        let seed_index = blocks
            .iter()
            .position(|b| b.kind == kaijutsu_types::BlockKind::Text && b.content.contains("It has run."))
            .expect("the seed block");
        assert!(
            seed_index > output_index,
            "the seed must land after the filled output block"
        );
    });
}

/// An approval belongs to the performer who raised it. If that performer is
/// replaced before a model turn's answer arrives, the old approval must settle
/// the waiting pair without execution and be spent; restoring the old
/// performer cannot make it redeemable again.
#[test]
fn a_turn_pair_with_a_changed_performer_settles_without_execution_or_reuse() {
    run_local(async {
        let scratch = Scratch::new("stale-performer");
        let s = seats().await;
        let marker = scratch.marker();
        let code = format!("echo stale-performer-ran > {}", marker.display());

        let ask = s.raise(&code).await;
        let (command_block_id, output_block_id) =
            s.link_waiting_pair(&ask, &code, PairOwner::Turn);

        let (original_performer, reviewer) = {
            let db = s.kernel.kernel_db.lock();
            let row = db.get_context(s.worker).unwrap().expect("worker context");
            (row.played_by.expect("worker performer"), row.reviewer_id.expect("worker reviewer"))
        };
        let replacement = PrincipalId::new();
        s.kernel
            .kernel_db
            .lock()
            .insert_character(&kaijutsu_kernel::kernel_db::CharacterRow {
                principal_id: replacement,
                name: "gate-replacement".into(),
                created_at: kaijutsu_types::now_millis() as i64,
                retired_at: None,
                handoff_ctx: None,
            })
            .unwrap();
        s.kernel
            .kernel_db
            .lock()
            .update_context_review(s.worker, Some(replacement), Some(reviewer))
            .unwrap();

        s.answer(&ask, true).await;
        wait_for("the stale turn pair to settle", || {
            s.block(&output_block_id).status != Status::Waiting
        })
        .await;

        assert_eq!(s.block(&command_block_id).status, Status::Error);
        assert_eq!(s.block(&output_block_id).status, Status::Error);
        assert!(
            s.block(&output_block_id)
                .stderr
                .unwrap_or_default()
                .contains("performer changed"),
            "the pair must name the changed performer"
        );
        assert!(!marker.exists(), "a stale approval must not execute its command");
        assert!(
            !s.undelivered(&ask),
            "the stale approved ask is consumed after settling its pair"
        );

        s.kernel
            .kernel_db
            .lock()
            .update_context_review(s.worker, Some(original_performer), Some(reviewer))
            .unwrap();
        let retry = s.raise(&code).await;
        assert_ne!(retry, ask, "restoring the performer must require a new approval");
        assert!(
            !s.decided(&retry),
            "the retry is a new pending ask, not a reused answer"
        );
        assert!(!marker.exists(), "the retry remains gated until it is answered");
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
        let (_command_block_id, output_block_id) = s.link_waiting_pair(&ask, &code, PairOwner::Session);

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
        let (command_block_id, output_block_id) = s.link_waiting_pair(&ask, &code, PairOwner::Session);

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

        // The seed says who approved and that it ran — no block id and no
        // "do NOT call it again": the output blocks reach the model as new
        // blocks in its own context.
        wait_for("the seed block saying it ran", || {
            s.worker_blocks().iter().any(|b| {
                b.kind == kaijutsu_types::BlockKind::Text
                    && b.content.contains("approved the action")
                    && b.content.contains("It has run.")
            })
        })
        .await;

        let seed = s
            .worker_blocks()
            .into_iter()
            .find(|b| b.kind == kaijutsu_types::BlockKind::Text && b.content.contains("It has run."))
            .expect("the seed block");
        assert!(
            !seed.content.contains("Nothing has run yet"),
            "the executed case must not tell the model to retry, got: {}",
            seed.content
        );
        assert!(
            !seed.content.starts_with("A human"),
            "the seed names the answerer, got: {}",
            seed.content
        );
        assert!(
            !seed.content.contains("do NOT"),
            "no shouting at the model, got: {}",
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
        let (command_block_id, output_block_id) = s.link_waiting_pair(&ask, &code, PairOwner::Session);
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
        // driver busy. Its performer cannot approve the ask, so the assigned
        // reviewer does — same as every other answer here.
        let kj = &s.worker_kj;
        let blocker_ctx = kj.create_context("gate-exec-blocker").await.unwrap();
        let reviewer = s
            .kernel
            .kernel_db
            .lock()
            .get_context(s.worker)
            .unwrap()
            .and_then(|row| row.reviewer_id)
            .expect("worker has its explicit reviewer");
        let actor = s
            .kernel
            .kernel_db
            .lock()
            .get_context(s.worker)
            .unwrap()
            .and_then(|row| row.played_by)
            .expect("worker has its explicit performer");
        s.kernel.kernel_db.lock().update_context_review(blocker_ctx, Some(actor), Some(reviewer)).unwrap();
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

/// The shipped path, whole: a command typed at the shell box is refused by
/// an asking hook, its ask carries the command as `exec_source` and names
/// the pair the refusal left `Waiting`, and the human's allow fills THAT
/// pair — no second pair beside it, and no seed block, because the player
/// who typed it is watching the block they typed into.
///
/// Falsified by dropping the `link_ask_blocks` call in
/// `execute_shell_command` (the driver authors a fresh pair and the typed
/// one stays `Waiting` forever), or by `hook_gate` losing `exec_source`
/// for a shell-shaped call (the answer becomes a wake and nothing runs).
#[test]
fn shell_box_pair_fills_when_its_own_ask_is_allowed() {
    run_local(async {
        let scratch = Scratch::new("shellbox-allow");
        let s = seats().await;
        s.install_ask_hook_on_worker().await;
        let marker = scratch.marker();
        let code = format!(
            "echo shell-box-ran >> {}\ncat {}",
            marker.display(),
            marker.display()
        );

        let (ask, command_block_id, output_block_id) = s.submit_from_shell_box(&code).await;
        assert!(!marker.exists(), "a refused submission must not have run");

        let row = s
            .kernel
            .kernel_db
            .lock()
            .get_approval(&ask)
            .unwrap()
            .expect("the ask row");
        assert_eq!(
            row.exec_source.as_deref(),
            Some(code.trim()),
            "a shell-box ask carries the typed command as its exec_source"
        );
        assert_eq!(
            row.command_block_id.as_deref(),
            Some(command_block_id.to_key().as_str()),
            "the ask names the command block waiting on it"
        );
        assert_eq!(
            row.output_block_id.as_deref(),
            Some(output_block_id.to_key().as_str()),
            "the ask names the output block waiting on it"
        );

        let blocks_before = s.worker_blocks().len();
        s.answer(&ask, true).await;

        wait_for("the shell box's own output block to settle", || {
            s.block(&output_block_id).status != Status::Waiting
        })
        .await;

        assert_eq!(s.block(&output_block_id).status, Status::Done);
        assert_eq!(s.block(&command_block_id).status, Status::Done);
        assert!(
            s.block(&output_block_id).content.contains("shell-box-ran"),
            "the typed pair's output block must hold the stdout, got {:?}",
            s.block(&output_block_id).content
        );
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "shell-box-ran\n");
        assert!(!s.undelivered(&ask), "an executed ask is redeemed");

        // Give a seed or a second pair time to appear if the driver were
        // going to author one; then insist it did not.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let blocks = s.worker_blocks();
        assert_eq!(
            blocks.len(),
            blocks_before,
            "a run into the pair the caller authored must author nothing else"
        );
        assert!(
            !blocks.iter().any(|b| b.content.contains("It has run.")),
            "a run into a caller-authored pair tells nobody"
        );
    });
}

/// The denied half of the same path: the pair the shell box left `Waiting`
/// settles `Error`, and nothing runs.
///
/// Falsified by settling only the output block, or by running the command
/// before reading the answer.
#[test]
fn shell_box_pair_settles_error_when_its_own_ask_is_denied() {
    run_local(async {
        let scratch = Scratch::new("shellbox-deny");
        let s = seats().await;
        s.install_ask_hook_on_worker().await;
        let marker = scratch.marker();
        let code = format!("echo shell-box-ran >> {}", marker.display());

        let (ask, command_block_id, output_block_id) = s.submit_from_shell_box(&code).await;
        s.answer(&ask, false).await;

        wait_for("the denied pair to settle", || {
            s.block(&output_block_id).status != Status::Waiting
        })
        .await;

        assert_eq!(s.block(&output_block_id).status, Status::Error);
        assert_eq!(s.block(&command_block_id).status, Status::Error);
        assert!(!marker.exists(), "a denied submission must not run");
        assert!(!s.undelivered(&ask), "a denial delivered into its pair is redeemed");
    });
}
