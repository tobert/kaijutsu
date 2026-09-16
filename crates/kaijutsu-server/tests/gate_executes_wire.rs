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
use common::{create_context};

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::run_local;
use kaijutsu_client::{KernelHandle, KeySource, RpcClient, RpcError, SshClient, SshConfig};
use kaijutsu_kernel::mcp::{AskSpec, GlobPattern, HookAction, HookEntry, HookId};
use kaijutsu_kernel::PairOwner;
use kaijutsu_server::{AuthDb, SharedKernel, SshServer, SshServerConfig};
use kaijutsu_types::{BlockId, BlockKind, ContextId, PrincipalId, Status, ToolKind};
use kaijutsu_types::shell_envelope::ShellStatus;
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

impl Seats {
    async fn close(self) {
        let Self {
            _worker_client, _approver_client, worker_kj, approver_kj, kernel, worker: _, approver: _,
        } = self;
        drop(worker_kj);
        drop(approver_kj);
        drop(_worker_client);
        drop(_approver_client);
        drop(kernel);
        tokio::task::yield_now().await;
    }
}

async fn seats() -> Seats {
    let tmp = tempfile::tempdir().unwrap();
    let auth_db_path = tmp.path().join("auth.db");
    let amy_principal = PrincipalId::new();
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
            principal_id: amy_principal,
            name: "amy".into(),
            created_at: kaijutsu_types::now_millis() as i64,
            retired_at: None,
            handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        db.insert_character(&kaijutsu_kernel::kernel_db::CharacterRow {
            principal_id: worker_principal,
            name: "gate-worker".into(),
            created_at: kaijutsu_types::now_millis() as i64,
            retired_at: None,
            handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
        db.insert_character(&kaijutsu_kernel::kernel_db::CharacterRow {
            principal_id: approver_principal,
            name: "gate-approver".into(),
            created_at: kaijutsu_types::now_millis() as i64,
            retired_at: None,
            handoff_ctx: None, root_ctx: None, root: false,
        }).unwrap();
    }
    let connect = |key: PrivateKey| async move {
        let config = SshConfig { host: addr.ip().to_string(), port: addr.port(), username: "gate".into(), key_source: KeySource::InMemory(Arc::new(key)), insecure: true };
        let mut ssh = SshClient::new(config);
        let channel = ssh.connect().await.unwrap();
        let mut client = RpcClient::new(channel.into_stream()).await.unwrap();
        client.retain_ssh_session(ssh);
        client
    };
    let worker_client = connect(worker_key).await;
    let approver_client = connect(approver_key).await;
    let (worker_kj, _) = worker_client.bind_kernel().await.unwrap();
    let (approver_kj, _) = approver_client.bind_kernel().await.unwrap();
    let worker = create_context(&worker_kj, "gate-exec-worker").await.unwrap();
    let approver = create_context(&approver_kj, "gate-exec-approver").await.unwrap();
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
            .call_mcp_tool("shell_write", &serde_json::json!({
                "command": command,
                "foreground": true,
            }))
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

    /// Withdraw from the actor seat. Cancellation is terminal without
    /// granting permission, but the subscriber still has to settle and
    /// deliver a linked turn pair.
    async fn cancel(&self, request_id: &str) {
        let kj = &self.worker_kj;
        kj.join_context(self.worker, "gate-exec-worker").await.unwrap();
        kj.shell_execute(
            &format!("kj ledger cancel {request_id}"),
            self.worker,
            true,
        )
        .await
        .expect("shell_execute for cancellation");
        wait_for("the cancellation to land in the ledger", || {
            matches!(
                self.kernel.kernel_db.lock().get_approval(request_id).unwrap(),
                Some(row) if row.status == kaijutsu_kernel::ApprovalStatus::Abandoned
            )
        })
        .await;
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

#[test]
fn shutdown_keeps_the_approved_turns_delivery_seed() {
    run_local(async {
        let scratch = Scratch::new("shutdown-seed");
        let s = seats().await;
        let marker = scratch.marker();
        let code = format!("echo entered > {}\nsleep 10", marker.display());
        let ask = s.raise(&code).await;
        s.answer(&ask, true).await;
        wait_for("approved execution to enter", || {
            std::fs::read_to_string(&marker).is_ok_and(|content| content == "entered\n")
        }).await;
        tokio::time::timeout(std::time::Duration::from_secs(2),
            s.kernel.kernel.shutdown_command_worker()).await.unwrap().unwrap();
        assert!(s.worker_blocks().iter().any(|block| block.kind == BlockKind::Text
            && block.content.contains("approved the action")),
            "shutdown discarded the durable delivery seed for the spent approval");
        assert!(!s.undelivered(&ask));
        s.close().await;
    });
}

#[test]
fn shutdown_settles_an_approved_command_before_returning() {
    run_local(async {
        let scratch = Scratch::new("shutdown");
        let s = seats().await;
        let marker = scratch.marker();
        let code = format!("echo entered > {}\nsleep 10\necho finished >> {}",
            marker.display(), marker.display());
        let ask = s.raise(&code).await;
        let (command, output) = s.link_waiting_pair(&ask, &code, PairOwner::Session);
        s.answer(&ask, true).await;
        wait_for("approved execution to enter", || {
            std::fs::read_to_string(&marker).is_ok_and(|content| content == "entered\n")
        }).await;
        tokio::time::timeout(std::time::Duration::from_secs(2),
            s.kernel.kernel.shutdown_command_worker()).await
            .expect("shutdown must cancel the approved command").unwrap();
        assert!(matches!(s.block(&output).status, Status::Done | Status::Error),
            "shutdown returned before the approved output settled");
        assert!(matches!(s.block(&command).status, Status::Done | Status::Error));
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "entered\n",
            "the cancelled command must not run its final statement");
        assert!(!s.undelivered(&ask));
        s.close().await;
    });
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
        s.close().await;
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
        s.close().await;
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
                handoff_ctx: None, root_ctx: None, root: false,
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
        s.close().await;
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
        s.close().await;
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
        s.close().await;
    });
}

/// A model's `Turn` pair is an in-place edit that its cached mailbox cannot
/// see. Terminal refusal therefore needs a new seed saying the command did
/// not run; a session-owned pair above deliberately stays settle-only.
#[test]
fn a_denied_turn_pair_tells_the_model_it_did_not_run() {
    run_local(async {
        let scratch = Scratch::new("denied-turn");
        let s = seats().await;
        let marker = scratch.marker();
        let code = format!("echo should-not-run > {}", marker.display());
        let ask = s.raise(&code).await;
        let (_command, output) = s.link_waiting_pair(&ask, &code, PairOwner::Turn);

        s.answer(&ask, false).await;
        wait_for("the denied turn pair to settle", || s.block(&output).status == Status::Error).await;
        wait_for("the no-run denial seed", || {
            s.worker_blocks().iter().any(|block| {
                block.kind == BlockKind::Text
                    && block.content.contains("denied the action")
                    && block.content.contains("It did NOT run.")
                    && block.content.contains(&output.to_key())
            })
        })
        .await;
        assert!(!marker.exists(), "a denied turn action must not run");
        assert!(!s.undelivered(&ask), "the delivered denial is redeemed");
        s.close().await;
    });
}

/// Cancellation follows the same delivery path as denial: it settles the
/// waiting model pair, is spent, and explicitly says that no command ran.
#[test]
fn a_cancelled_turn_pair_tells_the_model_it_did_not_run() {
    run_local(async {
        let scratch = Scratch::new("cancelled-turn");
        let s = seats().await;
        let marker = scratch.marker();
        let code = format!("echo should-not-run > {}", marker.display());
        let ask = s.raise(&code).await;
        let (_command, output) = s.link_waiting_pair(&ask, &code, PairOwner::Turn);

        s.cancel(&ask).await;
        wait_for("the cancelled turn pair to settle", || s.block(&output).status == Status::Error).await;
        wait_for("the no-run cancellation seed", || {
            s.worker_blocks().iter().any(|block| {
                block.kind == BlockKind::Text
                    && block.content.contains("cancelled the action")
                    && block.content.contains("It did NOT run.")
                    && block.content.contains(&output.to_key())
            })
        })
        .await;
        assert!(!marker.exists(), "a cancelled turn action must not run");
        assert!(!s.undelivered(&ask), "the delivered cancellation is redeemed");
        s.close().await;
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
fn default_async_shell_write_waits_then_approval_fills_its_operation_pair() {
    run_local(async {
        let scratch = Scratch::new("async-wait");
        let s = seats().await;
        let marker = scratch.marker();
        let code = format!("echo async-approved > {}", marker.display());

        s.worker_kj.join_context(s.worker, "gate-exec-worker").await.unwrap();
        let receipt = s
            .worker_kj
            .call_mcp_tool("shell_write", &serde_json::json!({ "command": code }))
            .await
            .expect("default async shell_write returns a waiting receipt");
        assert!(!receipt.is_error, "waiting is not an execution error: {receipt:?}");
        let body: serde_json::Value = serde_json::from_str(&receipt.content)
            .expect("shell receipt is JSON");
        assert_eq!(body["status"], "waiting");
        let operation_id = body["operation_id"].as_str().expect("stable operation id");
        let ask_id = body["ask_id"].as_str().expect("waiting receipt names ask");
        let operation = s
            .kernel
            .kernel
            .shell_operations()
            .get(operation_id, s.worker)
            .unwrap()
            .expect("waiting operation is durable");
        assert_eq!(operation.receipt.ask_id.as_deref(), Some(ask_id));
        assert!(operation.receipt.job_id.is_none(), "approval must precede kaish execution");
        assert_eq!(s.block(&operation.receipt.command_block_id).status, Status::Waiting);
        assert_eq!(s.block(&operation.receipt.output_block_id).status, Status::Waiting);

        s.answer(ask_id, true).await;
        wait_for("approved async operation to settle", || {
            s.kernel
                .kernel
                .shell_operations()
                .get(operation_id, s.worker)
                .unwrap()
                .is_some_and(|state| state.completed_at.is_some())
        })
        .await;
        let completed = s
            .kernel
            .kernel
            .shell_operations()
            .get(operation_id, s.worker)
            .unwrap()
            .expect("completed operation remains readable");
        assert_eq!(completed.envelope.expect("operation result").status, ShellStatus::Done);
        assert_eq!(s.block(&operation.receipt.command_block_id).status, Status::Done);
        assert_eq!(s.block(&operation.receipt.output_block_id).status, Status::Done);
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "async-approved\n");
        s.close().await;
    });
}

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
        s.close().await;
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
        s.close().await;
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
        let blocker_ctx = create_context(&kj, "gate-exec-blocker").await.unwrap();
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
            .call_mcp_tool("shell_write", &serde_json::json!({
                "command": "/bin/sleep 5",
                "foreground": true,
            }))
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
        s.close().await;
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
        s.close().await;
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
        s.close().await;
    });
}

struct CountResultHook(Arc<std::sync::atomic::AtomicUsize>);

#[async_trait::async_trait]
impl kaijutsu_kernel::mcp::Hook for CountResultHook {
    async fn invoke(&self, _: &kaijutsu_kernel::mcp::KernelCallParams,
        _: &kaijutsu_kernel::mcp::CallContext) -> kaijutsu_kernel::mcp::McpResult<()> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

async fn result_review_case(on_error: bool, allow: bool, script: bool, twice: bool) {
    let scratch = Scratch::new("post-call-review");
    let s = seats().await;
    let marker = scratch.marker();
    let code = if on_error {
        format!("echo once >> '{}'; if [[ nope -gt 2 ]]; then echo unexpected; fi", marker.display())
    } else { format!("echo once >> '{}'", marker.display()) };
    let observed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut hooks = s.kernel.kernel.broker().hooks().write().await;
    let table = if on_error { &mut hooks.on_error } else { &mut hooks.post_call };
    table.entries.push(HookEntry {
        id: HookId("observe-once".into()), match_instance: None,
        match_tool: Some(GlobPattern("shell_write".into())), match_context: Some(s.worker),
        match_principal: None, action: HookAction::Invoke(kaijutsu_kernel::mcp::HookBody::Builtin {
            name: "observe-once".into(), hook: Arc::new(CountResultHook(observed.clone())),
        }), priority: -1, kaish_script_id: None,
    });
    table.entries.push(HookEntry {
        id: HookId("review-result".into()), match_instance: None,
        match_tool: Some(GlobPattern("shell_write".into())), match_context: Some(s.worker),
        match_principal: None, action: if script {
            HookAction::Invoke(kaijutsu_kernel::mcp::HookBody::Kaish("echo review >&2; exit 3".into()))
        } else { HookAction::Ask(AskSpec { description: Some("Review captured result".into()) }) },
        priority: 0, kaish_script_id: None,
    });
    if twice {
        table.entries.push(HookEntry {
            id: HookId("second-review".into()), match_instance: None,
            match_tool: Some(GlobPattern("shell_write".into())), match_context: Some(s.worker),
            match_principal: None, action: HookAction::Ask(AskSpec { description: Some("Second result review".into()) }),
            priority: 1, kaish_script_id: None,
        });
    }
    table.entries.push(HookEntry {
        id: HookId("after-review".into()), match_instance: None,
        match_tool: Some(GlobPattern("shell_write".into())), match_context: Some(s.worker),
        match_principal: None, action: HookAction::ShortCircuit(kaijutsu_kernel::mcp::KernelToolResult::text("reviewed result")),
        priority: 2, kaish_script_id: None,
    });
    drop(hooks);
    s.worker_kj.join_context(s.worker, "post-call-review").await.unwrap();
    let submission = s.worker_kj.shell_submit(&code, s.worker, true).await.unwrap();
    wait_for("the result review ask", || {
        s.kernel.kernel_db.lock().list_pending_asks().unwrap().iter()
            .any(|ask| ask.hook_id.as_deref() == Some("review-result"))
    }).await;
    let ask = s.kernel.kernel_db.lock().list_pending_asks().unwrap().into_iter()
        .find(|ask| ask.hook_id.as_deref() == Some("review-result")).unwrap();
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "once\n");
    assert!(ask.exec_source.is_none(), "a result review must never authorize execution of its original command");
    assert_eq!(ask.origin.as_str(), "hook_result");
    s.answer(&ask.request_id, allow).await;
    if twice {
        wait_for("the second result review", || {
            s.kernel.kernel_db.lock().list_pending_asks().unwrap().iter()
                .any(|ask| ask.hook_id.as_deref() == Some("second-review"))
        }).await;
        let second = s.kernel.kernel_db.lock().list_pending_asks().unwrap().into_iter()
            .find(|ask| ask.hook_id.as_deref() == Some("second-review")).unwrap();
        assert_ne!(second.request_id, ask.request_id);
        s.answer(&second.request_id, true).await;
    }
    wait_for("reviewed operation completion", || {
        s.kernel.kernel.shell_operations().get(&submission.operation_id, s.worker).unwrap().unwrap().completed_at.is_some()
    }).await;
    let completed = s.kernel.kernel.shell_operations().get(&submission.operation_id, s.worker).unwrap().unwrap();
    let envelope = completed.envelope.unwrap();
    if allow {
        assert_eq!(envelope.stdout, "reviewed result", "remaining hooks must resume after approval");
        assert_eq!(envelope.status, ShellStatus::Done);
    } else {
        assert_eq!(envelope.status, ShellStatus::Error);
        assert_ne!(envelope.stdout, "reviewed result", "denial stops remaining hooks");
    }
    let captured = s.kernel.kernel.shell_operations().outcome(&submission.operation_id, s.worker).unwrap().unwrap();
    let first_review = s.kernel.kernel.shell_operations().result_review_for_ask(&ask.request_id, s.worker).unwrap().unwrap();
    assert_eq!(first_review.operation_id.as_deref(), Some(submission.operation_id.as_str()));
    assert!(first_review.settled.is_some());
    assert_eq!(s.kernel.kernel.shell_operations().get_by_ask(&ask.request_id, s.worker).unwrap().unwrap().receipt.operation_id,
        submission.operation_id, "earlier asks keep their operation link after sequential reviews");
    assert_eq!(matches!(captured.execution, kaijutsu_kernel::runtime::command_outcome::CommandExecution::Fault(_)), on_error);
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "once\n");
    assert_eq!(observed.load(std::sync::atomic::Ordering::SeqCst), 1, "earlier hooks must not rerun");
    s.close().await;
}

#[test]
fn post_call_approval_continues_hooks_without_executing_again() {
    run_local(result_review_case(false, true, false, false));
}

#[test]
fn denied_result_review_stops_remaining_hooks() {
    run_local(result_review_case(false, false, false, false));
}

#[test]
fn on_error_approval_continues_after_the_captured_fault() {
    run_local(result_review_case(true, true, false, false));
}

#[test]
fn kaish_result_hook_escalation_resumes_after_approval() {
    run_local(result_review_case(false, true, true, false));
}

#[test]
fn sequential_result_reviews_keep_the_same_hook_snapshot() {
    run_local(result_review_case(false, true, false, true));
}

#[test]
fn shutdown_settles_quiet_and_authored_structured_result_reviews() {
    run_local(async {
        for quiet in [false, true] {
            let s = seats().await;
            s.kernel.kernel.broker().hooks().write().await.post_call.entries.push(HookEntry {
                id: HookId("structured-shutdown-review".into()), match_instance: None,
                match_tool: Some(GlobPattern("shell_write".into())), match_context: Some(s.worker),
                match_principal: None, action: HookAction::Ask(AskSpec { description: Some("Review kj shutdown result".into()) }),
                priority: 0, kaish_script_id: None,
            });
            let argv = ["block", "create", "--role", "user", "--kind", "text", "--content", "structured-before-shutdown"]
                .into_iter().map(str::to_owned).collect::<Vec<_>>();
            let error = if quiet { s.worker_kj.execute_kj_quiet(s.worker, &argv).await }
                else { s.worker_kj.execute_kj(s.worker, &argv).await }.unwrap_err();
            let kaijutsu_client::RpcError::Refused(refusal) = error else { panic!("expected pending review: {error}") };
            assert_eq!(refusal.kind, kaijutsu_types::RefusalKind::Pending);
            let ask = refusal.ask.unwrap().request_id;
            tokio::time::timeout(std::time::Duration::from_secs(3), s.kernel.kernel.shutdown_command_worker())
                .await.expect("shutdown must settle retained structured review").unwrap();
            assert_eq!(s.kernel.kernel_db.lock().get_approval(&ask).unwrap().unwrap().status, kaijutsu_kernel::ApprovalStatus::Abandoned);
            let review = s.kernel.kernel.shell_operations().result_review_for_ask(&ask, s.worker).unwrap().unwrap();
            assert_eq!(review.operation_id.is_none(), quiet);
            let outcome = review.settled.expect("shutdown left structured review unsettled");
            assert_eq!(outcome.block_status(), Status::Error);
            assert!(matches!(outcome.execution, kaijutsu_kernel::runtime::command_outcome::CommandExecution::Completed(_)),
                "cancellation must retain already captured execution");
            assert_eq!(s.worker_blocks().iter().filter(|block| block.content == "structured-before-shutdown").count(), 1);
            s.close().await;
        }
    });
}

#[test]
fn structured_result_review_survives_rpc_disconnect() {
    run_local(async {
        let s = seats().await;
        s.worker_kj.join_context(s.worker, "structured-retained-review").await.unwrap();
        s.kernel.kernel.broker().hooks().write().await.post_call.entries.push(HookEntry {
            id: HookId("structured-retained-review".into()), match_instance: None,
            match_tool: Some(GlobPattern("shell_write".into())), match_context: Some(s.worker),
            match_principal: None, action: HookAction::Ask(AskSpec { description: Some("Review retained kj result".into()) }),
            priority: 0, kaish_script_id: None,
        });
        let argv = ["block", "create", "--role", "user", "--kind", "text", "--content", "retained-structured-once"]
            .into_iter().map(str::to_owned).collect::<Vec<_>>();
        let error = s.worker_kj.execute_kj(s.worker, &argv).await.unwrap_err();
        let kaijutsu_client::RpcError::Refused(refusal) = error else { panic!("expected pending review: {error}") };
        assert_eq!(refusal.kind, kaijutsu_types::RefusalKind::Pending);
        let ask = refusal.ask.unwrap().request_id;
        let operation = s.kernel.kernel.shell_operations().get_by_ask(&ask, s.worker).unwrap().unwrap();
        let session = *s.kernel.session_contexts.iter().find(|entry| *entry.value() == s.worker).unwrap().key();
        let Seats { _worker_client, _approver_client, worker_kj, approver_kj, kernel, worker, approver } = s;
        drop(worker_kj);
        drop(_worker_client);
        wait_for("structured submitting session to close", || !kernel.session_contexts.contains_key(&session)).await;
        let answer = approver_kj.execute_kj_quiet(approver, &["ledger".into(), "allow".into(), ask]).await.unwrap();
        assert_eq!(answer.exit_code, 0, "{}", answer.stderr);
        wait_for("structured review completion after disconnect", || kernel.kernel.shell_operations()
            .get(&operation.receipt.operation_id, worker).unwrap().unwrap().completed_at.is_some()).await;
        let blocks = kernel.documents.block_snapshots(worker).unwrap();
        assert_eq!(blocks.iter().filter(|block| block.content == "retained-structured-once").count(), 1);
        assert_eq!(blocks.iter().find(|block| block.id == operation.receipt.output_block_id).unwrap().status, Status::Done);
        drop(approver_kj);
        drop(_approver_client);
        kernel.kernel.shutdown_command_worker().await.unwrap();
    });
}

#[test]
fn structured_result_review_returns_pending_then_continues_without_repeating_kj() {
    run_local(async {
        let s = seats().await;
        s.kernel.kernel.broker().hooks().write().await.post_call.entries.push(HookEntry {
            id: HookId("structured-review".into()), match_instance: None,
            match_tool: Some(GlobPattern("shell_write".into())), match_context: Some(s.worker),
            match_principal: None, action: HookAction::Ask(AskSpec { description: Some("Review kj result".into()) }),
            priority: 0, kaish_script_id: None,
        });
        let argv: Vec<String> = ["block", "create", "--role", "user", "--kind", "text", "--content", "structured-review-once"]
            .into_iter().map(str::to_owned).collect();
        let error = tokio::time::timeout(std::time::Duration::from_secs(5),
            s.worker_kj.execute_kj(s.worker, &argv)).await.expect("result review must release the RPC").unwrap_err();
        let kaijutsu_client::RpcError::Refused(refusal) = error else { panic!("expected typed pending review: {error}") };
        assert_eq!(refusal.kind, kaijutsu_types::RefusalKind::Pending);
        let ask = refusal.ask.unwrap();
        let operation = s.kernel.kernel.shell_operations().get_by_ask(&ask.request_id, s.worker)
            .unwrap().expect("structured review has a durable receipt");
        assert_eq!(s.block(&operation.receipt.output_block_id).status, Status::Waiting);
        s.answer(&ask.request_id, true).await;
        wait_for("structured result publication", || {
            s.kernel.kernel.shell_operations().get(&operation.receipt.operation_id, s.worker)
                .unwrap().unwrap().completed_at.is_some()
        }).await;
        let output = s.block(&operation.receipt.output_block_id);
        assert_eq!(output.status, Status::Done, "{output:?}");
        let blocks = s.kernel.documents.block_snapshots(s.worker).unwrap();
        assert_eq!(blocks.iter().filter(|block| block.content == "structured-review-once").count(), 1);
        s.close().await;
    });
}

#[test]
fn quiet_result_review_keeps_its_result_without_a_transcript_pair() {
    run_local(async {
        use kaijutsu_kernel::mcp::{KernelToolResult, ToolContent};
        let s = seats().await;
        let help = s.approver_kj.execute_kj_quiet(s.approver,
            &["ledger".into(), "show".into(), "--help".into()]).await.unwrap();
        println!("Published ledger show help:\n{}", help.stdout);
        assert!(help.stdout.contains("captured execution"), "{}", help.stdout);
        let before = s.kernel.documents.block_snapshots(s.worker).unwrap().len();
        let replacement = serde_json::json!({"reviewed": "quiet"});
        let mut hooks = s.kernel.kernel.broker().hooks().write().await;
        for (id, action, priority) in [
            ("quiet-review", HookAction::Ask(AskSpec { description: Some("Review quiet result".into()) }), 0),
            ("quiet-replacement", HookAction::ShortCircuit(KernelToolResult {
                is_error: false, content: vec![ToolContent::Text("quiet result approved".into())],
                structured: Some(replacement.clone()),
            }), 1),
        ] {
            hooks.post_call.entries.push(HookEntry {
                id: HookId(id.into()), match_instance: None,
                match_tool: Some(GlobPattern("shell_write".into())), match_context: Some(s.worker),
                match_principal: None, action, priority, kaish_script_id: None,
            });
        }
        drop(hooks);
        let argv: Vec<String> = ["block", "create", "--role", "user", "--kind", "text", "--content", "quiet-review-once"]
            .into_iter().map(str::to_owned).collect();
        let error = tokio::time::timeout(std::time::Duration::from_secs(5),
            s.worker_kj.execute_kj_quiet(s.worker, &argv)).await.expect("quiet review must release the RPC").unwrap_err();
        let kaijutsu_client::RpcError::Refused(refusal) = error else { panic!("expected a review refusal: {error}") };
        assert_eq!(refusal.kind, kaijutsu_types::RefusalKind::Pending);
        let ask = refusal.ask.unwrap();
        let pending = s.approver_kj.execute_kj_quiet(s.approver,
            &["ledger".into(), "show".into(), ask.request_id.clone()]).await.unwrap().data.unwrap();
        assert!(pending["result_review"]["settled"].is_null());
        assert!(pending["result_review"]["operation_id"].is_null());
        assert_eq!(pending["result_review"]["captured"]["status"], "done");
        assert_eq!(s.kernel.documents.block_snapshots(s.worker).unwrap().len(), before + 1,
            "quiet execution authors only the block explicitly requested by the verb");
        s.answer(&ask.request_id, true).await;
        let settled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let shown = s.approver_kj.execute_kj_quiet(s.approver,
                    &["ledger".into(), "show".into(), ask.request_id.clone()]).await.unwrap().data.unwrap();
                if !shown["result_review"]["settled"].is_null() { break shown; }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.expect("quiet result remains inspectable after approval");
        assert_eq!(settled["result_review"]["settled"]["stdout"], "quiet result approved");
        assert_eq!(settled["result_review"]["settled"]["data"], replacement);
        let blocks = s.kernel.documents.block_snapshots(s.worker).unwrap();
        assert_eq!(blocks.len(), before + 1);
        assert_eq!(blocks.iter().filter(|block| block.content == "quiet-review-once").count(), 1);
        s.close().await;
    });
}

async fn streaming_output(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<kaijutsu_client::OutputEvent>, id: u64,
) -> (String, String, i32) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut stdout = String::new();
        let mut stderr = String::new();
        loop {
            match rx.recv().await.expect("output subscription closed") {
                kaijutsu_client::OutputEvent::Stdout { exec_id, text } if exec_id == id => stdout.push_str(&text),
                kaijutsu_client::OutputEvent::Stderr { exec_id, text } if exec_id == id => stderr.push_str(&text),
                kaijutsu_client::OutputEvent::ExitCode { exec_id, code } if exec_id == id => return (stdout, stderr, code),
                event => panic!("unexpected output: {event:?}"),
            }
        }
    }).await.expect("streaming command must finish")
}

#[test]
fn streaming_hooks_replace_output_in_every_phase() {
    run_local(async {
        for phase in ["pre", "post", "error"] {
            let scratch = Scratch::new("stream-replacement");
            let s = seats().await;
            s.worker_kj.join_context(s.worker, "stream-replacement").await.unwrap();
            let mut rx = s.worker_kj.subscribe_output().await.unwrap();
            let marker = scratch.marker();
            let mut hooks = s.kernel.kernel.broker().hooks().write().await;
            let table = match phase { "pre" => &mut hooks.pre_call, "post" => &mut hooks.post_call, _ => &mut hooks.on_error };
            table.entries.push(HookEntry {
                id: HookId("stream-replacement".into()), match_instance: None,
                match_tool: Some(GlobPattern("shell_write".into())), match_context: Some(s.worker),
                match_principal: None, priority: 0, kaish_script_id: None,
                action: HookAction::ShortCircuit(kaijutsu_kernel::mcp::KernelToolResult::text("settled stream")),
            });
            drop(hooks);
            let code = format!("echo once >> '{}'; echo raw; echo warning >&2; {}", marker.display(),
                if phase == "error" { "if [[ nope -gt 2 ]]; then echo unexpected; fi" } else { "false" });
            let id = s.worker_kj.execute(&code).await.unwrap();
            assert_eq!(streaming_output(&mut rx, id).await, ("settled stream".into(), String::new(), 0), "{phase}");
            if phase == "pre" { assert!(!marker.exists()); }
            else { assert_eq!(std::fs::read_to_string(marker).unwrap(), "once\n"); }
            s.close().await;
        }
    });
}

#[test]
fn streaming_result_reviews_wait_for_answer_or_interrupt_without_reexecution() {
    run_local(async {
        for decision in ["allow", "deny", "interrupt"] {
            let scratch = Scratch::new("stream-review");
            let s = seats().await;
            s.worker_kj.join_context(s.worker, "stream-review").await.unwrap();
            let before = s.worker_blocks().len();
            let mut rx = s.worker_kj.subscribe_output().await.unwrap();
            let marker = scratch.marker();
            let mut hooks = s.kernel.kernel.broker().hooks().write().await;
            for (id, action, priority) in [
                ("stream-review", HookAction::Ask(AskSpec { description: Some("Review stream".into()) }), 0),
                ("stream-reviewed", HookAction::ShortCircuit(kaijutsu_kernel::mcp::KernelToolResult::text("approved stream")), 1),
            ] {
                hooks.post_call.entries.push(HookEntry {
                    id: HookId(id.into()), match_instance: None, match_tool: Some(GlobPattern("shell_write".into())),
                    match_context: Some(s.worker), match_principal: None, action, priority, kaish_script_id: None,
                });
            }
            drop(hooks);
            let id = s.worker_kj.execute(&format!("echo once >> '{}'; echo captured", marker.display())).await.unwrap();
            wait_for("stream result review", || s.kernel.kernel_db.lock().list_pending_asks().unwrap().iter()
                .any(|ask| ask.hook_id.as_deref() == Some("stream-review"))).await;
            let ask = s.kernel.kernel_db.lock().list_pending_asks().unwrap().into_iter()
                .find(|ask| ask.hook_id.as_deref() == Some("stream-review")).unwrap();
            assert!(ask.exec_source.is_none());
            assert!(rx.try_recv().is_err(), "no output before the result review settles");
            assert!(s.worker_kj.execute("echo concurrent").await.is_err(), "review retains the execution slot");
            match decision {
                "interrupt" => s.worker_kj.interrupt(id).await.unwrap(),
                _ => s.answer(&ask.request_id, decision == "allow").await,
            }
            let (stdout, stderr, exit) = streaming_output(&mut rx, id).await;
            if decision == "allow" { assert_eq!((stdout, stderr, exit), ("approved stream".into(), String::new(), 0)); }
            else { assert!(stdout.is_empty()); assert!(!stderr.is_empty()); assert_ne!(exit, 0); }
            let review = s.kernel.kernel.shell_operations().result_review_for_ask(&ask.request_id, s.worker).unwrap().unwrap();
            assert!(review.operation_id.is_none());
            assert!(review.settled.is_some());
            assert_eq!(std::fs::read_to_string(marker).unwrap(), "once\n");
            assert_eq!(s.worker_blocks().len(), before, "stream review authors no transcript pair");
            s.close().await;
        }
    });
}

#[test]
fn streaming_output_limit_preserves_the_command_exit() {
    run_local(async {
        let s = seats().await;
        s.worker_kj.join_context(s.worker, "stream-spill").await.unwrap();
        let mut rx = s.worker_kj.subscribe_output().await.unwrap();
        let id = s.worker_kj.execute("seq 1 5000").await.unwrap();
        let (stdout, _, code) = streaming_output(&mut rx, id).await;
        assert!(stdout.contains("[output truncated"), "the output limit must be visible");
        assert_eq!(code, 0, "spilling output does not change the command's physical exit");
        s.close().await;
    });
}

#[test]
fn disconnect_settles_retained_stream_review_and_removes_the_session() {
    run_local(async {
        let s = seats().await;
        s.worker_kj.join_context(s.worker, "disconnect-review").await.unwrap();
        s.kernel.kernel.broker().hooks().write().await.post_call.entries.push(HookEntry {
            id: HookId("disconnect-review".into()), match_instance: None,
            match_tool: Some(GlobPattern("shell_write".into())), match_context: Some(s.worker),
            match_principal: None, priority: 0, kaish_script_id: None,
            action: HookAction::Ask(AskSpec { description: Some("Review before disconnect".into()) }),
        });
        s.worker_kj.execute("echo retained-before-disconnect").await.unwrap();
        wait_for("review before disconnect", || s.kernel.kernel_db.lock().list_pending_asks().unwrap().iter()
            .any(|ask| ask.hook_id.as_deref() == Some("disconnect-review"))).await;
        let ask = s.kernel.kernel_db.lock().list_pending_asks().unwrap().into_iter()
            .find(|ask| ask.hook_id.as_deref() == Some("disconnect-review")).unwrap();
        let session = *s.kernel.session_contexts.iter().find(|entry| *entry.value() == s.worker).unwrap().key();
        let Seats { _worker_client, _approver_client, worker_kj, approver_kj, kernel, worker, .. } = s;
        drop(worker_kj);
        drop(_worker_client);
        wait_for("disconnected review settlement", || kernel.kernel.shell_operations()
            .result_review_for_ask(&ask.request_id, worker).unwrap().unwrap().settled.is_some()).await;
        let review = kernel.kernel.shell_operations().result_review_for_ask(&ask.request_id, worker).unwrap().unwrap();
        assert_eq!(review.settled.unwrap().block_status(), Status::Error);
        assert_eq!(kernel.kernel_db.lock().get_approval(&ask.request_id).unwrap().unwrap().status,
            kaijutsu_kernel::ApprovalStatus::Abandoned);
        assert!(!kernel.session_contexts.contains_key(&session));
        let kaijutsu_kernel::runtime::command_outcome::CommandExecution::Completed(raw) = review.captured.execution
            else { panic!("disconnect lost captured execution") };
        assert_eq!(raw.text_out(), "retained-before-disconnect\n");
        drop(approver_kj);
        drop(_approver_client);
        tokio::task::yield_now().await;
    });
}

#[test]
fn mcp_result_review_survives_rpc_disconnect() {
    run_local(async {
        use kaijutsu_kernel::mcp::{Capability, ContextToolBinding, KernelToolResult};
        let s = seats().await;
        let mut binding = ContextToolBinding::new();
        binding.grant(Capability::Facade("shell".into()));
        s.kernel.kernel.broker().set_binding(s.worker, binding).await.unwrap();
        s.worker_kj.join_context(s.worker, "mcp-retained-review").await.unwrap();
        let mut hooks = s.kernel.kernel.broker().hooks().write().await;
        for (id, action, priority) in [
            ("mcp-retained-review", HookAction::Ask(AskSpec { description: Some("Review retained MCP output".into()) }), 0),
            ("mcp-reviewed", HookAction::ShortCircuit(KernelToolResult::text("reviewed after disconnect")), 1),
        ] {
            hooks.post_call.entries.push(HookEntry { id: HookId(id.into()), match_instance: None,
                match_tool: Some(GlobPattern("shell".into())), match_context: Some(s.worker), match_principal: None,
                action, priority, kaish_script_id: None });
        }
        drop(hooks);
        let receipt = s.worker_kj.call_mcp_tool("shell", &serde_json::json!({"command": "echo captured MCP output"})).await.unwrap();
        let body: serde_json::Value = serde_json::from_str(&receipt.content).unwrap();
        assert_eq!(body["status"], "running");
        let operation = body["operation_id"].as_str().unwrap().to_owned();
        wait_for("MCP result checkpoint", || s.kernel.kernel.shell_operations().get(&operation, s.worker).unwrap().unwrap()
            .receipt.ask_id.is_some()).await;
        let ask = s.kernel.kernel.shell_operations().get(&operation, s.worker).unwrap().unwrap().receipt.ask_id.unwrap();
        let session = *s.kernel.session_contexts.iter().find(|entry| *entry.value() == s.worker).unwrap().key();
        let Seats { _worker_client, _approver_client, worker_kj, approver_kj, kernel, worker, approver } = s;
        drop(worker_kj);
        drop(_worker_client);
        wait_for("MCP submitting session to close", || !kernel.session_contexts.contains_key(&session)).await;
        let answer = approver_kj.execute_kj_quiet(approver, &["ledger".into(), "allow".into(), ask.clone()]).await.unwrap();
        assert_eq!(answer.exit_code, 0, "{}", answer.stderr);
        wait_for("MCP review completion after disconnect", || kernel.kernel.shell_operations()
            .get(&operation, worker).unwrap().unwrap().completed_at.is_some()).await;
        let result = kernel.kernel.shell_operations().get(&operation, worker).unwrap().unwrap().envelope.unwrap();
        assert_eq!(result.stdout, "reviewed after disconnect");
        assert_eq!(result.status, ShellStatus::Done);
        let captured = kernel.kernel.shell_operations().outcome(&operation, worker).unwrap().unwrap();
        let kaijutsu_kernel::runtime::command_outcome::CommandExecution::Completed(raw) = captured.execution
            else { panic!("lost MCP execution") };
        assert_eq!(raw.text_out(), "captured MCP output\n");
        drop(approver_kj);
        drop(_approver_client);
        tokio::task::yield_now().await;
    });
}
