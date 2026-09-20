//! e2e: a working session, the way `docs/character.md`, "A session, inside
//! kaijutsu" describes it, driven end to end with a real mock model on every
//! seat — director and coders alike — through `KJ_MOCK_SCRIPT_DIR`
//! (`crates/kaijutsu-kernel/src/llm/mod.rs`'s `MockClient::with_script_dir`).
//!
//! Amy sits in banto's seat and submits one prompt. banto (the mock
//! `mock-banto` model) forks two coder lanes and assigns each a performer
//! and a distinct model itself (`mock-coder-a` / `mock-coder-b` — one
//! `MockClient` per-model queue per lane, so their concurrently-driven
//! turns can never interleave each other's scripted events) — a director
//! casting its own children (guidance, Amy 2026-09-15, "yes banto can
//! cast its own children");
//! banto then drives and waits on them, and each lane drifts a report back
//! to banto's seat. banto then notes a handoff, and a later
//! turn issues a gated statement that raises an ask; amy answers it as the
//! character responsible above banto's seat; a final turn signs off, closing the continuation
//! window; amy rotates banto's seat and reads the successor's instructions.
//!
//! **This scenario currently fails at the performer-assignment step** — see
//! "What this does not cover" below for the confirmed reason: it is a real
//! gap in `kj fork`, not a test-shape problem, and stays red on purpose
//! rather than being papered over.
//!
//! This binary has exactly one `#[test]` function. `KJ_MOCK_SCRIPT_DIR` is
//! process-wide state read once when the mock backend is constructed
//! (`Provider::from_backend`'s `BackendKind::Mock` arm) — a second test in
//! this file racing to set a different directory would be a real bug, not a
//! style question, so the whole scenario stays one test rather than several
//! that could interleave.
//!
//! ## What this does not cover
//!
//! - **A model rotating itself.** amy runs `kj context rotate banto` as
//!   banto's lineage root. banto running it from its own turn, and the ask
//!   a model raises when it lacks that authority (`docs/character.md`,
//!   "Roots and rotation"), are not exercised.
//! - **Reviewer resolution walks the context forest.** banto's seat is
//!   created from amy's own context, so the ask this scenario raises
//!   resolves to `amy` as the character responsible for that parent, and a
//!   lane's ask resolves to `banto` as the character responsible for the
//!   seat that forked it — see the assertions below and
//!   `docs/character.md`, "Roots and rotation".
//! - **The coder lanes' own `context_type`.** `kj fork` always copies the
//!   parent's `context_type` onto the child
//!   (`crates/kaijutsu-kernel/src/kj/fork.rs:1649-1686`,
//!   `inherit_parent_context_type`) — there is no `--type` on `fork`. Each
//!   lane therefore stays `context_type = "director"`, the same as banto's
//!   seat, so the cast's `coder` slot (keyed on `context_type`, per
//!   `crates/kaijutsu-kernel/src/model_resolution.rs:11-14`) is never
//!   reached by a lane's own type. Each lane instead gets an **explicit
//!   per-context model override** (`kj context set <lane> --model
//!   mock/mock-coder-a`, `-b` for the other), which the same module's resolution ladder
//!   (`model_resolution.rs:83-94`) always tries first — a real, exercised
//!   path, just not the cast-by-role path the brief sketched for the coder
//!   slot. The cast is still created and the `director` slot IS reached
//!   normally (banto's own `context_type` really is `director`).
//! - **banto casts its own lanes.** `kj context set <lane> --as <coder>`
//!   is allowed when the target's director is the caller
//!   (`crates/kaijutsu-kernel/src/kj/context.rs`, `caller_may_assign_performer`),
//!   and a fork is directed by the actor that forked it
//!   (`crates/kaijutsu-kernel/src/kj/fork.rs`), so the assignment happens
//!   inside banto's own scripted turn; amy assigns nothing.
//! - The kernel worker reserves `KAISH_RC_THREAD_STACK` for nested kaish
//!   (`crates/kaijutsu-kernel/src/runtime/worker.rs`), and
//!   `kj handoff note`/`signoff` without `--for` file under the performer
//!   (`crates/kaijutsu-kernel/src/kj/handoff.rs`, `resolve_caller_character`).

mod common;
use common::{create_context_typed};

use std::sync::Arc;
use std::time::Duration;

use common::{run_local, seed_mock_backend_with_model, shell_exec_wait};
use kaijutsu_client::{
    KernelHandle, KeySource, RpcClient, ServerEvent, SshClient, SshConfig, turn_events_channel,
};
use kaijutsu_server::{SharedKernel, SshServer, SshServerConfig};
use kaijutsu_types::{BlockKind, BlockQuery, ContextId, PrincipalId, Role, Status};
use russh::keys::PrivateKey;

/// Poll until `check` returns true, or fail loudly — every wait in this
/// file is on work a background driver does, so a timeout IS the bug
/// (`gate_executes_wire.rs`'s pattern).
async fn wait_for(label: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {label}");
}

async fn connect_with_key(addr: std::net::SocketAddr, key: PrivateKey, username: &str) -> RpcClient {
    let config = SshConfig {
        host: addr.ip().to_string(),
        port: addr.port(),
        username: username.to_string(),
        key_source: KeySource::InMemory(Arc::new(key)),
        insecure: true,
    };
    let mut ssh = SshClient::new(config);
    let channel = ssh.connect().await.expect("SSH connect");
    let mut client = RpcClient::new(channel.into_stream()).await.expect("RPC client init");
    client.retain_ssh_session(ssh);
    client
}

/// Drain the turn push channel until a terminal event for `ctx` arrives,
/// panicking on failure or a mismatched context — `user_input_identity.rs`'s
/// pattern.
async fn recv_turn_event(
    rx: &mut tokio::sync::broadcast::Receiver<ServerEvent>,
    ctx: ContextId,
) -> ServerEvent {
    loop {
        match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
            Ok(Ok(ev @ ServerEvent::TurnCompleted { context_id, .. }))
            | Ok(Ok(ev @ ServerEvent::TurnFailed { context_id, .. }))
                if context_id == ctx =>
            {
                return ev;
            }
            Ok(Ok(_)) => continue,
            Ok(Err(e)) => panic!("turn push channel error: {e}"),
            Err(_) => panic!("timed out waiting for a turn event on {ctx}"),
        }
    }
}

/// Fail loudly on a failed turn — a mock-scripted turn should never produce
/// one; a `TurnFailed` here means the script or the setup is wrong, not
/// something to shrug past.
fn expect_completed(event: ServerEvent, label: &str) {
    match event {
        ServerEvent::TurnCompleted { .. } => {}
        ServerEvent::TurnFailed { error, .. } => {
            panic!("{label}: turn failed instead of completing: {error}")
        }
        other => panic!("{label}: expected a terminal turn event, got {other:?}"),
    }
}

/// Everything the scenario needs: one authenticated connection for amy, the
/// live kernel handle for direct DB reads, and the two contexts she works
/// from.
struct Scenario {
    _amy_client: RpcClient,
    amy: KernelHandle,
    amy_principal: PrincipalId,
    kernel: SharedKernel,
    /// Amy's own out-of-band context: character/cast setup, rotation, and
    /// ledger answers all run here, never inside banto's context — so none
    /// of it can be caught by a hook scoped to banto's context id.
    amy_home: ContextId,
}

/// Boot a server with `KJ_MOCK_SCRIPT_DIR` pointed at this crate's
/// `tests/mock_scripts/` fixtures, a mock backend seeded (`mock-model` stays
/// the registry default; the scenario's contexts pick their own models via
/// cast slot / explicit override), and one real SSH credential bound to a
/// character named `amy`, whose context banto's seat is created from
/// (`docs/approval-identity.md`).
async fn boot() -> Scenario {
    // SAFETY: this binary has exactly one #[test] (see the module doc) —
    // nothing else in this process reads or writes this variable.
    unsafe {
        std::env::set_var(
            "KJ_MOCK_SCRIPT_DIR",
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/mock_scripts"),
        );
    }
    // Banto nests `kj drive` inside a tool call. The kernel worker uses
    // spawn_kaish_thread to reserve enough stack for this rc recursion.

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    // amy is the kernel's root character, as `kaijutsu-server init` makes
    // her, so she is the lineage root of every seat created from her work.
    let config = SshServerConfig::ephemeral_with_root(addr.port(), "amy");
    let amy_key = (*config.root_key()).clone();
    let amy_principal = kaijutsu_kernel::KernelDb::open(config.data_dir.as_ref().expect("ephemeral data dir").join("kernel.db"))
        .expect("open the ephemeral kernel.db")
        .get_character_by_name("amy")
        .expect("read amy's sheet")
        .expect("init created amy")
        .principal_id;
    if let Some(ref data_dir) = config.data_dir {
        seed_mock_backend_with_model(data_dir, "mock-model");
    }

    // `gate.toml`'s shipped default (`assets/defaults/gate.toml`) allow-lists
    // "kj handoff note" globally but has no `[context_type.director]` tier,
    // so banto's own `fork`/`context set`/`drive`/`wait`/`handoff signoff`
    // tool calls would each raise their own ask (`Uncovered` falls to `ask`,
    // `crates/kaijutsu-kernel/src/kj/gate_policy.rs`'s module doc) — the
    // "static allow tiers pass the routine ones" step of `docs/character.md`,
    // "A session, inside kaijutsu" step 3. This adds that tier so the
    // orchestration itself runs unattended; the plain `echo` statement later
    // in the script names no `kj` verb and so is never covered by any tier,
    // staying the one statement this scenario actually gates.
    let gate_toml_path = config
        .config_mounts
        .host_dir(kaijutsu_types::paths::CONFIG_ROOT)
        .join("gate.toml");
    std::fs::create_dir_all(gate_toml_path.parent().expect("gate.toml has a parent dir"))
        .expect("create /config/kernel dir");
    std::fs::write(
        &gate_toml_path,
        format!(
            "{}\n[context_type.director]\nallow = [\n  \"kj fork\",\n  \"kj context set\",\n  \"kj drive\",\n  \"kj wait\",\n  \"kj handoff signoff\",\n  \"kj drift push\",\n]\n",
            std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../assets/defaults/gate.toml"
            ))
            .expect("read shipped gate.toml")
        ),
    )
    .expect("write scenario gate.toml");

    let (kernel_tx, kernel_rx) = tokio::sync::oneshot::channel();
    tokio::task::spawn_local(async move {
        if let Err(e) = SshServer::new(config)
            .run_on_listener_with_kernel_sink(listener, kernel_tx)
            .await
        {
            log::error!("session_scenario server error: {e}");
        }
    });
    let kernel = kernel_rx.await.expect("server dropped the kernel handle");

    let amy_client = connect_with_key(addr, amy_key, "amy").await;
    let (amy, _) = amy_client.bind_kernel().await.expect("bind_kernel");
    // `director` rc grants `config-write` (character/cast administration)
    // and the `drive`/`fork`/`drift` verb authorities — amy's own admin
    // seat needs the same loadout banto's seat gets, not the bare "default"
    // type's narrower grant (`assets/defaults/rc/director/create/S10-binding.kai`).
    let amy_home = create_context_typed(&amy, "amy-home", "director")
        .await
        .expect("create amy-home");
    amy.join_context(amy_home, "amy").await.expect("join amy-home");

    Scenario {
        _amy_client: amy_client,
        amy,
        amy_principal,
        kernel,
        amy_home,
    }
}

impl Scenario {
    /// Run a `kj` command from amy's own context and return its output —
    /// panics on a non-`Done` status or a refusal, since every command this
    /// scenario runs from `amy_home` is expected to succeed outright.
    async fn kj(&self, code: &str) -> String {
        let (_id, output, status) = shell_exec_wait(&self.amy, code, self.amy_home).await;
        assert_eq!(
            status,
            Status::Done,
            "kj command {code:?} did not finish Done: output={output:?}"
        );
        output
    }

    fn context_by_label(&self, label: &str) -> ContextId {
        self.kernel
            .kernel_db
            .lock()
            .find_context_by_label(label)
            .unwrap_or_else(|e| panic!("looking up context {label:?}: {e}"))
            .unwrap_or_else(|| panic!("no context labeled {label:?}"))
            .context_id
    }

    fn character_principal(&self, name: &str) -> PrincipalId {
        self.kernel
            .kernel_db
            .lock()
            .list_characters(false)
            .expect("list characters")
            .into_iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no character named {name:?}"))
            .principal_id
    }

    fn blocks(&self, ctx: ContextId) -> Vec<kaijutsu_types::BlockSnapshot> {
        self.kernel
            .documents
            .block_snapshots(ctx)
            .unwrap_or_else(|e| panic!("reading blocks for {ctx}: {e}"))
    }
}

#[test]
fn kaijutsu_session_scenario() {
    run_local(async {
        let s = boot().await;

        // ------------------------------------------------------------
        // Setup: characters, cast, banto's seat.
        // ------------------------------------------------------------
        s.kj("kj character create banto").await;
        s.kj("kj character create coder-a").await;
        s.kj("kj character create coder-b").await;

        s.kj("kj cast create mockcast").await;
        s.kj("kj cast slot set mockcast director --backend mock --model mock-banto").await;
        s.kj("kj cast slot set mockcast coder --backend mock --model mock-coder").await;

        s.kj("kj context create banto --type director --as banto --cast mockcast").await;
        let banto_ctx = s.context_by_label("banto");
        let banto_principal = s.character_principal("banto");
        let coder_a_principal = s.character_principal("coder-a");
        let coder_b_principal = s.character_principal("coder-b");

        // ------------------------------------------------------------
        // Turn 1a: amy submits a prompt in banto's seat. banto's mock script
        // forks lane-a and lane-b, then assigns each lane's performer
        // itself (`kj context set lane-x --as coder-x`), as the director
        // that forked them (guidance, Amy 2026-09-15, "yes banto can cast
        // its own children"; `docs/approval-identity.md`, the assignment
        // paragraph).
        //
        // banto directs the lanes it forked, so banto's own turn assigns
        // their performers.
        // ------------------------------------------------------------
        let (callback, mut turn_rx) = turn_events_channel(64);
        s.amy.subscribe_turn_events(callback).await.expect("subscribe_turn_events");

        s.amy.join_context(banto_ctx, "amy-in-banto").await.expect("join banto");
        s.amy
            .edit_input(banto_ctx, 0, "kick off today's lanes", 0)
            .await
            .expect("edit_input");
        let submit = s.amy.submit_input(banto_ctx, false).await.expect("submit_input");

        let all = s.amy.get_blocks(banto_ctx, &BlockQuery::All).await.expect("get_blocks");
        let prompt_block = all
            .iter()
            .find(|b| b.id == submit.block_id)
            .expect("the submitted prompt block exists");
        assert_eq!(
            prompt_block.id.principal_id, s.amy_principal,
            "amy's prompt block must be authored by amy's connection principal"
        );

        expect_completed(recv_turn_event(&mut turn_rx, banto_ctx).await, "banto turn 1a (fork)");
        assert_eq!(
            s.kernel.kernel_db.lock().get_context(banto_ctx).unwrap().unwrap().played_by,
            Some(banto_principal),
            "banto's seat is played by banto"
        );

        let lane_a_ctx = s.context_by_label("lane-a");
        let lane_b_ctx = s.context_by_label("lane-b");
        for (label, ctx) in [("lane-a", lane_a_ctx), ("lane-b", lane_b_ctx)] {
            let row = s.kernel.kernel_db.lock().get_context(ctx).unwrap().unwrap();
            assert_eq!(row.forked_from, Some(banto_ctx), "{label}'s structural parent must be banto's seat");
        }

        // ------------------------------------------------------------
        // Turn 1b: banto drives and waits on both lanes.
        // ------------------------------------------------------------
        s.kj("kj drive banto --prompt go").await;
        expect_completed(recv_turn_event(&mut turn_rx, banto_ctx).await, "banto turn 1b (drive + wait)");

        // ------------------------------------------------------------
        // Assert: every banto text/tool-call block so far is authored by
        // banto's principal, not amy's (the requester) or the system's —
        // `docs/issues.md`, "Identity audit", item 5.
        // ------------------------------------------------------------
        let banto_blocks_after_turn1 = s.blocks(banto_ctx);
        let model_authored: Vec<_> = banto_blocks_after_turn1
            .iter()
            .filter(|b| matches!(b.kind, BlockKind::Text | BlockKind::ToolCall) && b.role == Role::Model)
            .collect();
        assert!(
            !model_authored.is_empty(),
            "banto's turn 1 must have produced at least one model text/tool-call block"
        );
        for b in &model_authored {
            assert_eq!(
                b.id.principal_id, banto_principal,
                "banto's model block {:?} (kind={:?}) must be authored by banto's principal, got {:?}",
                b.id, b.kind, b.id.principal_id
            );
        }

        // Assert: the lanes are played by the coder characters amy assigned.
        for (label, ctx, performer) in [
            ("lane-a", lane_a_ctx, coder_a_principal),
            ("lane-b", lane_b_ctx, coder_b_principal),
        ] {
            let row = s.kernel.kernel_db.lock().get_context(ctx).unwrap().unwrap();
            assert_eq!(
                row.forked_from,
                Some(banto_ctx),
                "{label}'s structural parent must be banto's seat"
            );
            assert_eq!(
                row.played_by,
                Some(performer),
                "{label} must be played by its assigned coder character"
            );

            let lane_blocks = s.blocks(ctx);
            // A full fork (`kj fork`'s default — no `--include`/`--exclude`
            // narrowing) shares the parent's history up to the fork point,
            // block ids and all (`docs/fork-filters.md`) — lane-a's log
            // therefore legitimately starts with banto's own pre-fork
            // blocks, the fork tool call among them. Only the blocks after
            // the lane's own seed (its `kj drive --prompt` text) are this
            // lane's actual turn; that is what "authored by its own
            // performer" pins.
            let own_seed = lane_blocks
                .iter()
                .rposition(|b| b.role == Role::User)
                .unwrap_or_else(|| panic!("{label} has no seed block to anchor its own turn"));
            let lane_model_blocks: Vec<_> = lane_blocks[own_seed + 1..]
                .iter()
                .filter(|b| matches!(b.kind, BlockKind::Text | BlockKind::ToolCall) && b.role == Role::Model)
                .collect();
            assert!(
                !lane_model_blocks.is_empty(),
                "{label} must have produced at least one model block after its own seed"
            );
            for b in &lane_model_blocks {
                assert_eq!(
                    b.id.principal_id, performer,
                    "{label}'s model block {:?} must be authored by its own performer, got {:?}",
                    b.id, b.id.principal_id
                );
            }
        }

        // Assert: each coder's `kj drift push` landed a report in banto's
        // seat.
        // `kj drift push` lands a `BlockKind::Drift` block — a plain
        // substring match on "report: done" also catches `kj wait`'s own
        // narrative tool-result text (RT4 tails both lanes' conversations,
        // "report: done" included), so the kind filter is load-bearing,
        // not decorative.
        let banto_blocks_after_lanes = s.blocks(banto_ctx);
        let report_blocks: Vec<_> = banto_blocks_after_lanes
            .iter()
            .filter(|b| b.kind == BlockKind::Drift && b.content.contains("report: done"))
            .collect();
        assert_eq!(
            report_blocks.len(), 2,
            "both lanes' drift-pushed reports must land in banto's seat, found {}",
            report_blocks.len()
        );

        // Each report is a drift block, so its sender is the performer that
        // ran `kj drift push` — the pushing lane's own coder character, not
        // amy's connection principal (`docs/approval-identity.md`, "Three
        // identities"; `kj/drift.rs`'s `deliver_drift`).
        let mut report_authors: Vec<PrincipalId> =
            report_blocks.iter().map(|b| b.author()).collect();
        report_authors.sort();
        let mut expected_authors = [coder_a_principal, coder_b_principal];
        expected_authors.sort();
        assert_eq!(
            report_authors, expected_authors,
            "each lane's drift-pushed report must be authored by its own performer, got {report_authors:?}"
        );
        assert!(
            !report_authors.contains(&s.amy_principal),
            "a drift-pushed report must never be authored by amy's connection principal, got {report_authors:?}"
        );

        // ------------------------------------------------------------
        // Turn 2: banto notes a handoff.
        // ------------------------------------------------------------
        s.kj("kj drive banto --prompt go").await;
        expect_completed(recv_turn_event(&mut turn_rx, banto_ctx).await, "banto turn 2 (handoff note)");

        let tail_after_note = s.kj("kj handoff tail banto").await;
        assert!(
            tail_after_note.contains("lanes landed"),
            "banto's handoff tail must contain the first note, got: {tail_after_note}"
        );

        // ------------------------------------------------------------
        // Turn 3: banto's gated statement raises an ask. `echo` names no
        // `kj` verb, so no tier in `gate.toml` — not even the
        // `[context_type.director]` one this scenario just wrote — ever
        // covers it; it is `Uncovered` and falls to the default `ask`
        // (`crates/kaijutsu-kernel/src/kj/gate_policy.rs`'s module doc).
        // No hook install needed: this is the kernel's real default gate
        // policy, the same one `gate_executes_wire.rs` and
        // `user_input_identity.rs` exercise.
        // ------------------------------------------------------------
        s.kj("kj drive banto --prompt go").await;
        expect_completed(recv_turn_event(&mut turn_rx, banto_ctx).await, "banto turn 3 (gated statement)");

        let amy_principal_id = s.amy_principal;
        let pending = s.kernel.kernel_db.lock().list_pending_asks().expect("list_pending_asks");
        let ask = pending
            .into_iter()
            .find(|a| a.exec_source.as_deref().unwrap_or("").contains("gate-approved-for-real"))
            .unwrap_or_else(|| panic!("banto's gated `echo` statement must have raised a pending ask"));
        assert_eq!(
            ask.context_id,
            banto_ctx.as_bytes().to_vec(),
            "the ask's context must be banto's seat"
        );
        assert_eq!(
            ask.actor_id.as_deref(),
            Some(banto_principal.as_bytes().as_slice()),
            "the ask's actor must be banto, the performer who called shell_write"
        );
        assert_eq!(
            ask.reviewer_id.as_deref(),
            Some(amy_principal_id.as_bytes().as_slice()),
            "the ask's reviewer resolves to amy by the walk: banto's seat \
             was created from amy's own context, and amy is the character \
             responsible for it"
        );

        // The same walk one level down: a lane's ask answers to banto, the
        // character responsible for the seat that forked it. Read through
        // the resolver rather than by raising a second ask, so the lanes'
        // scripted turns stay as they are.
        for (label, ctx, performer) in
            [("lane-a", lane_a_ctx, coder_a_principal), ("lane-b", lane_b_ctx, coder_b_principal)]
        {
            assert_eq!(
                s.kernel.kernel_db.lock().effective_approval_reviewer(ctx, performer).unwrap(),
                kaijutsu_kernel::kernel_db::EffectiveReviewer::Assigned(banto_principal),
                "{label}'s performer answers to banto, which forked it",
            );
        }

        // ------------------------------------------------------------
        // Amy answers the ask from her own context.
        // ------------------------------------------------------------
        s.kj(&format!("kj ledger allow {}", ask.request_id)).await;
        wait_for("the ask to be decided", || {
            matches!(
                s.kernel.kernel_db.lock().get_approval(&ask.request_id).unwrap(),
                Some(row) if row.status == kaijutsu_kernel::ApprovalStatus::Allowed
            )
        })
        .await;

        // The approval EXECUTES — the statement really ran as banto, not
        // just a recorded decision (`docs/gate-resume.md`, "Slice 5:
        // approval executes"): the ask's own linked output block fills in
        // with the command's real stdout.
        let output_block_id = ask
            .output_block_id
            .as_deref()
            .and_then(kaijutsu_types::BlockId::from_key)
            .expect("an executable ask links an output block");
        wait_for("the approved statement to actually execute", || {
            s.kernel
                .documents
                .get_block_snapshot(banto_ctx, &output_block_id)
                .ok()
                .flatten()
                .map(|b| b.status == Status::Done && b.content.contains("gate-approved-for-real"))
                .unwrap_or(false)
        })
        .await;

        // ------------------------------------------------------------
        // Turn 4: banto signs off — reached by the kernel's own automatic
        // resume, not another explicit `kj drive`. Approving the ask opens
        // a continuation epoch (`crates/kaijutsu-kernel/src/runtime/llm_stream.rs`'s
        // gate-resume path) and the approved command's completion resumes
        // banto's conversation on it directly: the mock queue's evidence
        // (the "handoff signoff" round trip was already consumed by the
        // time the next explicit `kj drive` ran, panicking the queue empty
        // when this test still issued one) is what corrected this section —
        // originally written assuming every turn boundary here was an
        // explicit drive. `docs/approval-identity.md`, "Continuation
        // windows and async work" names the mechanism; this scenario is the
        // evidence that an *approval*, not only a human's own next message,
        // is what resumes it.
        // ------------------------------------------------------------
        expect_completed(recv_turn_event(&mut turn_rx, banto_ctx).await, "banto turn 4 (auto-resumed signoff)");

        // A note without `--for` belongs to the performer, not the
        // requesting connection (`kj/handoff.rs`, `resolve_caller_character`).
        // banto's own signoff lands in banto's log although amy's
        // `submitInput` started the session.
        let banto_tail = s.kj("kj handoff tail banto").await;
        assert!(
            banto_tail.contains("lanes landed"),
            "the explicit-target note (`--for banto`) lands in banto's own log, got: {banto_tail}"
        );
        assert!(
            banto_tail.contains("rotating"),
            "banto's signoff without --for belongs to the performer, banto \
             (`kj/handoff.rs`, `resolve_caller_character` reads the actor), got: {banto_tail}"
        );
        let amy_tail = s.kj("kj handoff tail amy").await;
        assert!(
            !amy_tail.contains("rotating"),
            "the requesting connection's log must not receive the performer's note, got: {amy_tail}"
        );

        // ------------------------------------------------------------
        // Rotation (`docs/prompts.md`, "Rotating a context").
        // ------------------------------------------------------------
        let rotated = s.kj("kj context rotate banto").await;
        assert!(rotated.contains("rotated banto"), "rotate reports the replacement, got: {rotated}");
        let banto_next_ctx = s.context_by_label("banto");
        assert_ne!(banto_next_ctx, banto_ctx, "the label follows the live successor");
        {
            let db = s.kernel.kernel_db.lock();
            assert!(db.get_context(banto_ctx).unwrap().unwrap().archived_at.is_some(), "the predecessor is archived");
            let successor = db.get_context(banto_next_ctx).unwrap().unwrap();
            assert_eq!(successor.played_by, Some(banto_principal));
            assert_eq!(successor.context_type, "director");
        }

        // `S16-handoff.kai` injects the handoff tail as a `Notification`
        // block, not into the hydrated instruction text `kj context prompt`
        // renders (that surface covers the `(System, Text)` create-lifecycle
        // blocks — `docs/prompts.md` — plus runtime facts, not every
        // notification a create-time rc script leaves). The block log is
        // the durable record either way, so this checks it directly rather
        // than the narrower rendered surface.
        let banto_next_blocks = s.blocks(banto_next_ctx);
        let handoff_injection = banto_next_blocks
            .iter()
            .find(|b| b.content.contains("Handoff") && b.content.contains("auto-injected at create"))
            .unwrap_or_else(|| panic!("banto-next must have an injected handoff block"));
        assert!(
            handoff_injection.content.contains("lanes landed"),
            "the injected handoff must carry the note that actually reached banto's own \
             log, got: {}",
            handoff_injection.content
        );

        let predecessor_injection = banto_next_blocks
            .iter()
            .find(|b| b.content.contains("Rotated from context"))
            .unwrap_or_else(|| panic!("banto-next must have an injected predecessor excerpt"));
        assert!(
            predecessor_injection.content.contains("signed off."),
            "the predecessor excerpt should reach banto's last turn, got: {}",
            predecessor_injection.content
        );
    });
}
