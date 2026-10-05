//! Slice 5: toolie/director role bundles seeded via rc.
//!
//! These exercise the full path the capability-policy work delivers:
//! `kj context create --type toolie` runs the seeded rc `create` scripts,
//! whose `S10-binding.kai` calls `kj binding allow …` to narrow the new
//! context to a read-only allow-set — and that allow-set is then enforced at
//! `call_tool`. Running the real kaish scripts is the point: a malformed
//! capability token (e.g. a mislexed `instance:tool`) would leave the
//! expected grant absent and fail these assertions.
//!
//! Lives as an integration test (not a `#[cfg(test)]` unit test) so it
//! compiles against the library proper.

use std::sync::Arc;

use kaijutsu_kernel::block_store::shared_block_store_with_db;
use kaijutsu_kernel::drift::shared_drift_router;
use kaijutsu_kernel::kernel_db::KernelDb;
use kaijutsu_kernel::mcp::{Capability, CallContext, InstanceId, KernelCallParams, McpError};
use kaijutsu_kernel::{Kernel, KjCaller, KjDispatcher, KjResult};
use kaijutsu_types::{KernelId, PrincipalId, SessionId};
use tokio_util::sync::CancellationToken;

struct Harness {
    kernel: Arc<Kernel>,
    dispatcher: Arc<KjDispatcher>,
    db: Arc<parking_lot::Mutex<KernelDb>>,
    store: kaijutsu_kernel::block_store::SharedBlockStore,
    creator: PrincipalId,
    _tmp: tempfile::TempDir,
}

/// Build a kernel with builtins registered + a kj dispatcher over the same
/// DB (so seeded rc scripts, context rows, and bindings all line up). The
/// in-memory DB auto-seeds the role context_types on open.
async fn harness() -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let creator = PrincipalId::system();
    let db = Arc::new(parking_lot::Mutex::new(KernelDb::temporary().unwrap()));
    let ws_id = db.lock().get_or_create_default_workspace(creator).unwrap();
    let store = shared_block_store_with_db(db.clone(), ws_id, creator);

    let kernel = Arc::new(Kernel::new("rc-role-test", tmp.path(), store.clone(), db.clone()).await);

    // Seed and mount a private `/config/rc` tree — the same setup the server RPC
    // boot path (kaijutsu-server/src/rpc.rs) and the unit-test helper
    // (kj::test_helpers) perform. Without it, `load_scripts` hits
    // `NoMountPoint`, finds no scripts, and the role lifecycle never runs —
    // which is exactly what these tests exist to exercise.
    let rc_dir = tmp.path().join("rc");
    std::fs::create_dir_all(&rc_dir).expect("create rc dir");
    kaijutsu_kernel::seed_scripts::ensure_rc_seed_files(&rc_dir).expect("seed rc files");
    kernel
        .mount("/config/rc", kaijutsu_kernel::vfs::LocalBackend::new(&rc_dir))
        .await;

    let file_cache = kernel.file_cache().clone();
    kernel
        .register_builtin_mcp_servers(store.clone(), file_cache, None, db.clone())
        .await
        .expect("register_builtin_mcp_servers");

    let dispatcher = Arc::new(KjDispatcher::new(
        shared_drift_router(),
        store.clone(),
        db.clone(),
        kernel.clone(),
    ));
    dispatcher.set_self_arc();

    Harness {
        kernel,
        dispatcher,
        db,
        store,
        creator,
        _tmp: tmp,
    }
}

/// Mint a root character via `kj character create --root` and return its
/// root context id: the same `ensure_root_context` path boot runs
/// (`docs/fork-and-create.md`, "Where a created context sits"). A root
/// console is parentless, which `kj context create` never mints.
async fn ensure_root(h: &Harness, name: &str) -> kaijutsu_types::ContextId {
    let caller = KjCaller {
        principal_id: h.creator,
        actor_id: h.creator,
        reviewer_id: None,
        context_id: None,
        session_id: SessionId::new(),
        confirmed: false,
        rc_depth: 0,
        privileged: true,
        cancel: tokio_util::sync::CancellationToken::new(),
    };
    let argv: Vec<String> = ["character", "create", name, "--root"].iter().map(|s| s.to_string()).collect();
    let res = h.dispatcher.dispatch(&argv, &caller).await;
    assert!(matches!(res, KjResult::Ok { .. }), "character create --root failed: {}", res.message());
    h.db
        .lock()
        .get_character_by_name(name)
        .unwrap()
        .unwrap_or_else(|| panic!("character '{name}' not found"))
        .root_ctx
        .expect("character create --root must bind a root context")
}

/// Create a context of `context_type` via `kj context create`, firing its rc
/// `create` lifecycle, and return the new context id. The caller stands in
/// a root console, because `kj context create` needs a current context or
/// `--parent`.
async fn create_typed(h: &Harness, label: &str, context_type: &str) -> kaijutsu_types::ContextId {
    let root = ensure_root(h, "amy").await;
    // Unprivileged caller: the rc create lifecycle assigns the loadout via its
    // own privileged kaish (EmbeddedKaish::for_context), not this caller.
    let caller = KjCaller {
        principal_id: h.creator,
        actor_id: h.creator,
        reviewer_id: None,
        context_id: Some(root),
        session_id: SessionId::new(),
        confirmed: false,
        rc_depth: 0,
        privileged: false,
        cancel: tokio_util::sync::CancellationToken::new(),
    };
    let argv: Vec<String> = ["context", "create", label, "--type", context_type]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let res = h.dispatcher.dispatch(&argv, &caller).await;
    assert!(
        matches!(res, KjResult::Ok { .. }),
        "{context_type} context create failed: {}",
        res.message()
    );
    h.db
        .lock()
        .resolve_context(label)
        .unwrap_or_else(|e| panic!("context '{label}' not found: {e}"))
}

/// Run `body` to completion on a thread sized for rc lifecycles, joining it
/// and re-raising any panic. `create_typed` nests two rc lifecycles
/// (`ensure_root`'s root create, then the typed context's own create), each
/// re-entering kaish many levels deep on one stack — enough to overflow the
/// default ~2 MiB test-harness thread stack. Mirrors the in-crate
/// `run_on_rc_stack` helper (`kj/transport.rs`) and the server's own
/// beat-scheduler/SSH threads, which spawn through
/// [`kaijutsu_kernel::spawn_kaish_thread`] for the same reason.
fn run_on_rc_stack(body: impl FnOnce() + Send + 'static) {
    kaijutsu_kernel::spawn_kaish_thread("rc-test-thread", body)
        .expect("spawn rc-stack thread")
        .join()
        .expect("rc-stack thread panicked");
}

/// Thin wrapper so the facade assertions read cleanly.
async fn fx_broker_check(
    h: &Harness,
    ctx: &kaijutsu_types::ContextId,
    facade: &str,
) -> Result<(), McpError> {
    h.kernel.broker().check_facade(ctx, facade).await
}

#[test]
fn toolie_role_seeds_one_read_only_shell_and_refuses_writes() {
    run_on_rc_stack(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime")
            .block_on(toolie_role_seeds_one_read_only_shell_and_refuses_writes_body());
    });
}

async fn toolie_role_seeds_one_read_only_shell_and_refuses_writes_body() {
    let h = harness().await;
    let ctx = create_typed(&h, "exp", "toolie").await;

    let binding = h
        .kernel
        .broker()
        .binding(&ctx)
        .await
        .expect("toolie rc must seed a binding");
    let file = InstanceId::new("builtin.file");
    let block = InstanceId::new("builtin.block");

    // The toolie is an explorer modeled on kaibo's: one tool, the read-only
    // `shell`. Files, blocks, and kernel search are all reached through it.
    let call_ctx = CallContext::new(h.creator, ctx, SessionId::new(), KernelId::new());
    let visible: Vec<String> = h
        .kernel
        .broker()
        .list_visible_tools(ctx, &call_ctx)
        .await
        .expect("list the toolie's tools")
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(visible, ["shell"], "the toolie's roster is the read-only shell alone");
    for (instance, tool) in [(&file, "read"), (&file, "write"), (&file, "edit"), (&block, "block_read"),
        (&block, "block_create")] {
        assert!(!binding.allows_tool(instance, tool), "toolie must NOT hold {instance}:{tool}");
    }

    // Facades: toolie holds `facade:shell`, the read-only shell, and is
    // refused `facade:shell_write`. The RPC shell (the person's box,
    // `kaijutsu-mcp`) follows the facade, so it is read-only here too.
    assert!(!binding.is_admin(), "toolie must NOT be a binding admin");
    assert!(
        !binding.allows(&Capability::Editor),
        "toolie must NOT hold the editor write capability"
    );
    assert!(
        fx_broker_check(&h, &ctx, "shell").await.is_ok(),
        "toolie must hold the SAFE shell facade under the unmarked name"
    );
    assert!(
        matches!(
            fx_broker_check(&h, &ctx, "shell_write").await,
            Err(McpError::FacadeDenied { .. })
        ),
        "toolie shell_write facade must be refused"
    );
    assert!(
        matches!(
            h.kernel.broker().check_shell_facade(&ctx).await,
            Ok(kaijutsu_kernel::runtime::context_shell::ShellPolicy::ReadOnly)
        ),
        "toolie's RPC shell must be the read-only shell"
    );
    assert!(
        matches!(
            fx_broker_check(&h, &ctx, "submit_input").await,
            Err(McpError::FacadeDenied { .. })
        ),
        "toolie submit_input facade must be refused"
    );

    // Enforced at the call path: a write is refused, not silently dropped.
    let denied = h
        .kernel
        .broker()
        .call_tool(
            KernelCallParams {
                instance: file.clone(),
                tool: "write".into(),
                arguments: serde_json::json!({"path": "/x", "content": "y"}),
            },
            &call_ctx,
            CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(denied, Err(McpError::CapabilityDenied { .. })),
        "toolie write must be refused, got {denied:?}"
    );
}

#[test]
fn director_role_seeds_block_tooling_but_not_file_writes() {
    run_on_rc_stack(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime")
            .block_on(director_role_seeds_block_tooling_but_not_file_writes_body());
    });
}

async fn director_role_seeds_block_tooling_but_not_file_writes_body() {
    let h = harness().await;
    let ctx = create_typed(&h, "dir", "director").await;

    let binding = h
        .kernel
        .broker()
        .binding(&ctx)
        .await
        .expect("director rc must seed a binding");
    let file = InstanceId::new("builtin.file");
    let block = InstanceId::new("builtin.block");

    // Whole block instance granted — including mutating block tools.
    assert!(
        binding.allows_tool(&block, "block_create"),
        "director should allow block_create (whole-instance grant)"
    );
    assert!(binding.allows_tool(&block, "block_read"));
    // File access: read + the write tools. A director owns the rc lifecycle
    // scripts, so it gets file:write/edit for governance artifacts — general
    // code edits still delegate to a coder context. See
    // assets/defaults/rc/director/create/S10-binding.kai.
    assert!(binding.allows_tool(&file, "read"), "director should allow file read");
    assert!(
        binding.allows_tool(&file, "write"),
        "director should allow file write (rc-lifecycle governance)"
    );
    assert!(
        binding.allows_tool(&file, "edit"),
        "director should allow file edit (rc-lifecycle governance)"
    );
    // `/config/rc` carries no capability of its own — the file:write grant above
    // is what reaches it, enforced at the call path exactly like any other
    // write. A real write through the broker must succeed.
    let call_ctx = CallContext::new(h.creator, ctx, SessionId::new(), KernelId::new())
        .with_cwd(std::path::PathBuf::from("/"));
    let written = h
        .kernel
        .broker()
        .call_tool(
            KernelCallParams {
                instance: file.clone(),
                tool: "write".into(),
                arguments: serde_json::json!({
                    "path": "/config/rc/director/create/S99-write-test.kai",
                    "content": "true\n",
                }),
            },
            &call_ctx,
            CancellationToken::new(),
        )
        .await;
    assert!(
        written.is_ok(),
        "director's file:write must reach /config/rc with no extra capability: {written:?}"
    );

    // Director is a binding admin — may write any context's loadout.
    assert!(binding.is_admin(), "director should hold binding-admin");

    // Director holds the interactive editor's write capability alongside
    // the file-tool writes above (assets/defaults/rc/director/create/
    // S10-binding.kai grants it directly — not via the S10-lib symlink, so
    // this harness's host `LocalBackend` mount does exercise it).
    assert!(
        binding.allows(&Capability::Editor),
        "director should hold the editor write capability"
    );

    // Facades: director gets the full (collapsed) interaction surface.
    for facade in ["shell", "edit_input", "submit_input"] {
        assert!(
            binding.allows(&Capability::Facade(facade.to_string())),
            "director should allow facade {facade}"
        );
        assert!(
            fx_broker_check(&h, &ctx, facade).await.is_ok(),
            "director facade {facade} should pass the gate"
        );
    }
}

/// A root context is a human's admin console with no model: it holds the
/// operator's whole authority set and composes no instruction blocks.
///
/// A root context is minted at boot (`ensure_root_contexts`) or by `kj
/// character create --root` — never by `kj context create --type root`,
/// which would hang a "root"-typed context off another context as a plain
/// child instead of minting the parentless console the type means. This
/// exercises the real path via `ensure_root`, the same `ensure_root_context`
/// internals boot runs.
#[test]
fn root_role_is_a_model_less_admin_console() {
    run_on_rc_stack(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime")
            .block_on(root_role_is_a_model_less_admin_console_body());
    });
}

async fn root_role_is_a_model_less_admin_console_body() {
    let h = harness().await;
    let ctx = ensure_root(&h, "amy").await;

    let binding = h
        .kernel
        .broker()
        .binding(&ctx)
        .await
        .expect("root rc must seed a binding");
    assert!(binding.is_admin(), "root should hold binding-admin");
    for cap in [
        Capability::Operator,
        Capability::ConfigWrite,
        Capability::Drive,
        Capability::Fork,
        Capability::Drift,
        Capability::Transport,
        Capability::System,
        Capability::Exec,
        Capability::Editor,
        Capability::Facade("shell".into()),
        Capability::Facade("shell_write".into()),
        Capability::Facade("edit_input".into()),
        Capability::Facade("submit_input".into()),
    ] {
        assert!(binding.allows(&cap), "root should allow {cap:?}");
    }
    let file = InstanceId::new("builtin.file");
    assert!(binding.allows_tool(&file, "write"), "root should allow file write");
    assert!(binding.allows_tool(&InstanceId::new("builtin.block"), "block_create"));

    // Instructions are `(System, Text)` blocks; tool notifications and rc
    // traces are other kinds.
    let instructions: Vec<_> = h
        .store
        .block_snapshots(ctx)
        .expect("root context blocks")
        .into_iter()
        .filter(|block| block.role == kaijutsu_types::Role::System && block.kind == kaijutsu_types::BlockKind::Text)
        .collect();
    assert!(
        instructions.is_empty(),
        "a model-less root must compose no instruction blocks, got {:?}",
        instructions.iter().map(|block| block.content.chars().take(80).collect::<String>()).collect::<Vec<_>>()
    );
}

/// `kj character create --root` runs the root bundle's create lifecycle on
/// the root context it creates.
#[tokio::test]
async fn character_create_root_binds_its_root_context() {
    let h = harness().await;
    let caller = KjCaller {
        principal_id: h.creator,
        actor_id: h.creator,
        reviewer_id: None,
        context_id: None,
        session_id: SessionId::new(),
        confirmed: false,
        rc_depth: 0,
        privileged: true,
        cancel: tokio_util::sync::CancellationToken::new(),
    };
    let argv: Vec<String> = ["character", "create", "keeper", "--root"].iter().map(|s| s.to_string()).collect();
    let res = h.dispatcher.dispatch(&argv, &caller).await;
    assert!(matches!(res, KjResult::Ok { .. }), "{}", res.message());

    let root_ctx = h.db.lock().get_character_by_name("keeper").unwrap().unwrap().root_ctx.expect("root context");
    let binding = h.kernel.broker().binding(&root_ctx).await.expect("the root rc bundle must bind it");
    assert!(binding.is_admin(), "the root context must hold binding-admin");
}

#[test]
fn mcp_role_holds_config_governance() {
    run_on_rc_stack(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime")
            .block_on(mcp_role_holds_config_governance_body());
    });
}

async fn mcp_role_holds_config_governance_body() {
    // The `mcp` context_type is the producer/orchestrator voice (Claude Code
    // over MCP, cheaper than API rates). On top of the shared broad loadout it
    // adds the config governance cap via S15-governance.kai, so it can drive
    // the SQL-native model surfaces (`kj backend`/`kj cast`/`kj alias`) and
    // `kj hook add/remove`/`kj mcp reload`. `/config/rc` and `/config/kernel`
    // carry no capability of their own — the file:write grant from the
    // shared loadout (S10 → lib) already reaches them, same as any other file.
    //
    // NB: the broad loadout itself (S10 → lib via the rc composition symlink)
    // is NOT asserted here — this test only checks the config-governance cap
    // S15 adds on top of it. S15 is a plain script, so it runs and grants
    // regardless of whether the symlink resolved.
    let h = harness().await;
    let ctx = create_typed(&h, "mcp-role", "mcp").await;
    let binding = h
        .kernel
        .broker()
        .binding(&ctx)
        .await
        .expect("mcp rc must seed a binding");

    // The governance cap added by S15 — deny-by-default, NOT implied by '*'.
    assert!(
        binding.allows(&Capability::ConfigWrite),
        "mcp should hold config-write for model-config governance"
    );
}

/// Every command the toolie stance teaches, except the `git` reads, runs in
/// the toolie's shell, and each refusal it describes reads as it says. The
/// stance must carry each example verbatim, so a stance edit that changes an
/// example changes this test too. The `git` examples need a repository, which
/// this harness does not mount.
#[test]
fn the_toolie_stance_examples_run_in_its_read_only_shell() {
    run_on_rc_stack(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime")
            .block_on(the_toolie_stance_examples_run_in_its_read_only_shell_body());
    });
}

async fn the_toolie_stance_examples_run_in_its_read_only_shell_body() {
    let h = harness().await;
    h.kernel.broker().set_kj_dispatcher(&h.dispatcher).await;
    let ctx = create_typed(&h, "explorer", "toolie").await;
    let stance = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"),
        "/../../assets/defaults/rc/toolie/create/S00-stance.md")).expect("read the toolie stance");
    let call_ctx = CallContext::new(h.creator, ctx, SessionId::new(), KernelId::new());
    let run = |command: String| {
        let broker = h.kernel.broker().clone();
        let call_ctx = call_ctx.clone();
        async move {
            let result = broker.call_tool(KernelCallParams {
                instance: InstanceId::new("builtin.shell"),
                tool: "shell".into(),
                arguments: serde_json::json!({"command": command}),
            }, &call_ctx, CancellationToken::new()).await.expect("the shell call settles");
            result.structured.expect("the shell returns its envelope")
        }
    };
    // The stance's placeholders, bound to files every toolie seat has.
    let file = "/config/rc/toolie/create/S00-stance.md";
    let dir = "/config/rc/toolie";
    let reads = [
        ("cat -n FILE", format!("cat -n {file}")),
        ("wc -l FILE", format!("wc -l {file}")),
        ("cat -n FILE | sed -n '1,150p'", format!("cat -n {file} | sed -n '1,150p'")),
        ("grep -rn PATTERN src docs", format!("grep -rn explorer {dir}")),
        ("grep -rnF 'fn submit(' crates", format!("grep -rnF 'Read files' {dir}")),
        ("-B4 -A8", format!("grep -rn -B4 -A8 explorer {dir}")),
        ("file FILE", format!("file {file}")),
        ("pwd", "pwd".to_string()),
        ("kj context log", "kj context log".to_string()),
        ("kj block list", "kj block list".to_string()),
        ("kj search PATTERN", "kj search explorer".to_string()),
        ("help syntax", "help syntax".to_string()),
    ];
    for (example, command) in reads {
        assert!(stance.contains(&format!("`{example}`")), "the stance no longer shows `{example}`");
        let envelope = run(command.clone()).await;
        assert_eq!(envelope["exit_code"], 0, "{command}: {envelope}");
    }
    assert!(stance.contains("`kj block read ID`"), "the stance no longer shows `kj block read ID`");
    let listed = run("kj block list".to_string()).await;
    let id = listed["data"][0].as_str().expect("kj block list returns block ids").to_string();
    let envelope = run(format!("kj block read {id}")).await;
    assert_eq!(envelope["exit_code"], 0, "kj block read {id}: {envelope}");
    let refusals = [
        (format!("echo probe > {dir}/probe"), 1, "read-only filesystem"),
        ("kj drift push . probe".to_string(), 1, "read-only"),
        ("curl http://localhost/".to_string(), 1, "read-only"),
        ("/usr/bin/true".to_string(), 127, "read-only"),
    ];
    for (write, code, says) in refusals {
        let envelope = run(write.clone()).await;
        assert_eq!(envelope["exit_code"], code, "{write} must be refused: {envelope}");
        assert!(envelope["stderr"].as_str().unwrap_or_default().contains(says),
            "{write}: the refusal must say {says:?}: {envelope}");
    }
}

/// The `house` capability (the kj administration verbs) is a house seat's
/// grant and not a worker's. Each context type's real rc `create` lifecycle
/// assigns its loadout, so a grant that is mislexed or missing shows here.
#[test]
fn only_house_seats_hold_the_house_capability() {
    run_on_rc_stack(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime")
            .block_on(only_house_seats_hold_the_house_capability_body());
    });
}

async fn only_house_seats_hold_the_house_capability_body() {
    let h = harness().await;
    let root = ensure_root(&h, "amy").await;
    let mut seats = vec![("root", root)];
    for ty in ["default", "mcp", "director", "coder", "toolie", "musician"] {
        let ctx = create_typed(&h, &format!("seat-{ty}"), ty).await;
        seats.push((ty, ctx));
    }
    for (ty, ctx) in seats {
        let binding = h.kernel.broker().binding(&ctx).await
            .unwrap_or_else(|| panic!("{ty} rc must seed a binding"));
        let house = !matches!(ty, "coder" | "toolie" | "musician");
        assert_eq!(binding.allows(&Capability::House), house, "{ty}: house");
    }
}

/// A coder keeps the work authorities (drift, fork, exec, editor) and drops
/// the ones that run the instrument: the kj verbs for context lifecycle,
/// the beat, and self-driving belong to a house seat.
#[test]
fn coder_loadout_is_a_worker_loadout() {
    run_on_rc_stack(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime")
            .block_on(coder_loadout_is_a_worker_loadout_body());
    });
}

async fn coder_loadout_is_a_worker_loadout_body() {
    let h = harness().await;
    let ctx = create_typed(&h, "coder-seat", "coder").await;
    let binding = h.kernel.broker().binding(&ctx).await.expect("coder rc must seed a binding");
    for cap in [Capability::Drift, Capability::Fork, Capability::Exec, Capability::Editor,
                Capability::Facade("shell_write".into())] {
        assert!(binding.allows(&cap), "coder should allow {cap:?}");
    }
    for cap in [Capability::House, Capability::Operator, Capability::Transport,
                Capability::Drive, Capability::System, Capability::ConfigWrite, Capability::Admin] {
        assert!(!binding.allows(&cap), "coder must not allow {cap:?}");
    }
}
