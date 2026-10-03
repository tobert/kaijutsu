//! e2e: drive the `kaijutsu-solo-acp` binary as an ACP client would.
//!
//! Every test here spawns the real binary, which starts a real kernel in
//! process and serves ACP v1 on its stdin/stdout. The model is the scripted
//! mock backend (`KJ_MOCK_SCRIPT_DIR`, one script file per model name), so a
//! turn runs end to end with no provider and no spend.
//!
//! The client is the ACP fleet's (`kaijutsu_acp_fleet::client`,
//! `docs/acp-fleet.md`): it exposes raw notification shapes and counts what
//! reaches stdout, which a typed client hides.
//!
//! Scratch state lives under `/home/atobey/src/bench-work/solo/` — a real
//! disk, and under `$HOME/src`, which is the kernel's read-write VFS mount
//! (`crates/kaijutsu-server/src/rpc.rs`, the `$HOME/src` mount). A session
//! cwd outside that mount is read-only to the model, so the file-writing
//! test would fail for a reason that has nothing to do with this binary.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kaijutsu_acp_fleet::client::{self, AcpClient, AgentCommand};
use serde_json::Value;

/// The binary under test, built by cargo for this test target.
const BIN: &str = env!("CARGO_BIN_EXE_kaijutsu-solo-acp");

/// Where scratch state goes. Real disk: `/tmp` on this host is a small
/// tmpfs and a kernel database does not belong there.
const SCRATCH: &str = "/home/atobey/src/bench-work/solo";

/// A boot plus a first turn crosses an rc lifecycle and a model turn; 120s
/// is generous enough that a timeout means something is actually wedged.
const REPLY_TIMEOUT: Duration = Duration::from_secs(120);

/// A fresh directory under [`SCRATCH`], unique per call.
fn scratch_dir(label: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    let path = PathBuf::from(SCRATCH).join(format!("{label}-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&path).expect("create the scratch directory");
    path
}

/// One scripted model, by the directory holding `<model>.json`.
fn mock_scripts(case: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/mock_scripts")
        .join(case)
}

/// The binary with the mock backend and the scripted model `case`.
fn mock_command(case: &str) -> AgentCommand {
    AgentCommand::new(BIN)
        .arg("--backend-kind")
        .arg("mock")
        .arg("--model")
        .arg("solo-mock")
        .env("KJ_MOCK_SCRIPT_DIR", mock_scripts(case))
        // The default state directory is a temp dir; keep it off the
        // host's small /tmp tmpfs.
        .env("TMPDIR", scratch_dir("tmp"))
        .env("RUST_LOG", "info")
}

fn spawn(command: AgentCommand) -> AcpClient {
    AcpClient::spawn(&command, REPLY_TIMEOUT).unwrap_or_else(|e| panic!("spawn {BIN}: {e:#}"))
}

/// Spawn the binary with the mock backend and a scripted model.
fn spawn_mock(case: &str) -> AcpClient {
    spawn_mock_with(case, &[])
}

/// Spawn the binary with the mock backend, plus extra flags.
fn spawn_mock_with(case: &str, extra: &[&str]) -> AcpClient {
    spawn(extra.iter().fold(mock_command(case), |c, a| c.arg(*a)))
}

/// Spawn the binary from a chosen launch directory, which is what the
/// default workspace mount is about.
fn spawn_mock_from(case: &str, cwd: &Path, extra: &[&str]) -> AcpClient {
    spawn(extra.iter().fold(mock_command(case), |c, a| c.arg(*a)).cwd(cwd))
}

/// `initialize`, then `session/new` at `cwd`; the session id.
fn open_session(agent: &mut AcpClient, cwd: &Path) -> String {
    agent.initialize().expect("initialize");
    agent.new_session(cwd).expect("session/new")
}

/// Send one prompt and return its response.
fn prompt(agent: &mut AcpClient, session: &str, text: &str) -> Value {
    agent.prompt(session, text).unwrap_or_else(|e| panic!("{e:#}"))
}

/// Every `agent_message_chunk` text seen so far, joined.
fn agent_text(agent: &AcpClient) -> String {
    client::agent_text(agent.updates())
}

/// Close stdin, the way an ACP client shutting down does, and wait for the
/// exit.
fn close_stdin_and_wait(agent: &mut AcpClient, within: Duration) -> std::process::ExitStatus {
    agent.shutdown(within).unwrap_or_else(|e| panic!("{e:#}"))
}

fn wait_for_exit(agent: &mut AcpClient, after: &str, within: Duration) -> std::process::ExitStatus {
    agent.wait_exit(after, within).unwrap_or_else(|e| panic!("{e:#}"))
}

fn wait_for_stderr(agent: &AcpClient, needle: &str, within: Duration) {
    agent.wait_for_stderr(needle, within).unwrap_or_else(|e| panic!("{e:#}"));
}

/// Send a signal to the running agent, the way a service manager or a
/// Ctrl-C does.
fn signal(agent: &AcpClient, signal: i32) {
    let pid = agent.pid() as i32;
    // SAFETY: `kill(2)` on a child this process spawned and has not
    // reaped; an invalid pid would only return an error we check.
    let sent = unsafe { libc::kill(pid, signal) };
    assert_eq!(sent, 0, "kill({pid}, {signal}) failed");
}

/// The state directory the binary reports at startup, so a test can watch
/// it appear and (for a temp one) disappear.
fn state_dir_from(stderr: &str) -> PathBuf {
    for line in stderr.lines() {
        if let Some(rest) = line.split("solo state directory: ").nth(1) {
            return PathBuf::from(rest.trim());
        }
    }
    panic!("no 'solo state directory:' line on stderr:\n{stderr}");
}

/// Poll until `check` passes, failing loudly with stderr on timeout.
fn wait_for(label: &str, agent: &AcpClient, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!(
        "timed out waiting for {label}\n--- stderr ---\n{}",
        agent.stderr()
    );
}

#[test]
fn a_prompt_runs_a_turn_and_ends_it() {
    let cwd = scratch_dir("chat-cwd");
    let mut agent = spawn_mock("chat");

    let init = agent.initialize().expect("initialize");
    assert_eq!(
        init.get("protocolVersion").and_then(Value::as_u64),
        Some(1),
        "we speak ACP v1: {init}"
    );

    let session = agent.new_session(&cwd).expect("session/new");
    let response = prompt(&mut agent, &session, "say something");
    assert_eq!(
        response.get("stopReason").and_then(Value::as_str),
        Some("end_turn"),
        "a scripted turn ends cleanly: {response}"
    );
    assert!(
        agent_text(&agent).contains("solo mock speaking"),
        "the model's text reached the client as a chunk; saw {:?}",
        agent_text(&agent)
    );
}

#[test]
fn a_file_tool_call_writes_inside_the_session_cwd() {
    let cwd = scratch_dir("file-cwd");
    let mut agent = spawn_mock("file");

    let session = open_session(&mut agent, &cwd);
    let response = prompt(&mut agent, &session, "write the file");
    assert_eq!(
        response.get("stopReason").and_then(Value::as_str),
        Some("end_turn"),
        "the scripted tool turn ends cleanly: {response}"
    );

    // The script's `write` names a RELATIVE path, so this also proves the
    // ACP session cwd reached the kernel's file tool.
    let written = cwd.join("solo-wrote-this.txt");
    assert!(
        written.is_file(),
        "the file tool wrote nothing at {}\n--- stderr ---\n{}",
        written.display(),
        agent.stderr()
    );
    assert_eq!(
        std::fs::read_to_string(&written).expect("read what the model wrote"),
        "written by the solo kernel\n"
    );
}

/// A directory outside every fixed read-write mount, removed at the end of
/// the test. `/var/tmp` is deliberately neither `$HOME/src` nor `/tmp`:
/// `/home/atobey/src/bench-work` is under `$HOME/src` and is already
/// writable, so it could not tell a working mount from a missing one.
struct Outside(PathBuf);

impl Outside {
    fn new(label: &str) -> Self {
        let path = PathBuf::from("/var/tmp").join(format!(
            "kaijutsu-solo-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock before epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create a directory outside the fixed mounts");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn wrote(&self) -> bool {
        self.0.join("solo-wrote-this.txt").is_file()
    }
}

impl Drop for Outside {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_named_mount_makes_a_directory_writable() {
    let outside = Outside::new("mounted");
    let mut agent = spawn_mock_with(
        "file",
        &["--mount", outside.path().to_str().expect("utf-8 path")],
    );

    let session = open_session(&mut agent, outside.path());
    let response = prompt(&mut agent, &session, "write the file");
    assert_eq!(
        response.get("stopReason").and_then(Value::as_str),
        Some("end_turn"),
        "the scripted tool turn ends cleanly: {response}"
    );

    assert!(
        outside.wrote(),
        "a --mount directory must be writable by the model's file tools\n\
         --- stderr ---\n{}",
        agent.stderr()
    );
    assert_eq!(
        std::fs::read_to_string(outside.path().join("solo-wrote-this.txt"))
            .expect("read what the model wrote"),
        "written by the solo kernel\n",
        "the bytes are on the host disk, not only in the kernel"
    );
}

#[test]
fn an_unmounted_directory_stays_read_only() {
    let outside = Outside::new("unmounted");
    // Launched from the crate directory (under $HOME/src, already writable),
    // so the default cwd mount adds nothing and this session cwd is reached
    // only through the read-only root.
    let mut agent = spawn_mock("file");

    let session = open_session(&mut agent, outside.path());
    prompt(&mut agent, &session, "write the file");

    assert!(
        !outside.wrote(),
        "without a mount the model must not reach outside the perimeter"
    );
}

#[test]
fn the_launch_cwd_is_mounted_by_default() {
    let outside = Outside::new("launch-cwd");
    let mut agent = spawn_mock_from("file", outside.path(), &[]);

    let session = open_session(&mut agent, outside.path());
    prompt(&mut agent, &session, "write the file");

    assert!(
        outside.wrote(),
        "the directory the agent was launched in is its workspace\n\
         --- stderr ---\n{}",
        agent.stderr()
    );
}

#[test]
fn no_cwd_mount_leaves_the_launch_directory_read_only() {
    let outside = Outside::new("no-cwd-mount");
    let mut agent = spawn_mock_from("file", outside.path(), &["--no-cwd-mount"]);

    let session = open_session(&mut agent, outside.path());
    prompt(&mut agent, &session, "write the file");

    assert!(
        !outside.wrote(),
        "--no-cwd-mount means what it says\n--- stderr ---\n{}",
        agent.stderr()
    );
}

#[test]
fn an_unusable_mount_refuses_the_boot() {
    let missing = Outside::new("missing").path().join("not-here");
    let cases: [(&str, &[&str]); 4] = [
        ("absolute", &["--mount", "work/project"]),
        (
            "does not exist",
            &["--mount", missing.to_str().expect("utf-8 path")],
        ),
        ("read-only root", &["--mount", "/"]),
        ("reserved", &["--mount", "/config"]),
    ];
    for (reason, args) in cases {
        let output = Command::new(BIN)
            .arg("--backend-kind")
            .arg("mock")
            .arg("--model")
            .arg("solo-mock")
            .args(args)
            .env("TMPDIR", scratch_dir("tmp"))
            .output()
            .unwrap_or_else(|e| panic!("run {args:?}: {e}"));
        assert!(
            !output.status.success(),
            "{args:?} must refuse the boot, got {:?}",
            output.status
        );
        assert!(
            output.stdout.is_empty(),
            "stdout stays empty on a refusal: {:?}",
            String::from_utf8_lossy(&output.stdout)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(reason),
            "{args:?} must say why ({reason}):\n{stderr}"
        );
        assert!(
            stderr.contains(args[1]),
            "{args:?} must name the path:\n{stderr}"
        );
    }
}

/// `--unlist` reaches the kernel's boot check: a relative path refuses the
/// start and is named.
#[test]
fn an_unusable_unlist_refuses_the_boot() {
    let output = Command::new(BIN)
        .args(["--backend-kind", "mock", "--model", "solo-mock", "--unlist", "logs"])
        .env("TMPDIR", scratch_dir("tmp"))
        .output()
        .unwrap_or_else(|e| panic!("run --unlist logs: {e}"));
    assert!(!output.status.success(), "a relative --unlist must refuse the boot");
    assert!(output.stdout.is_empty(), "stdout stays empty on a refusal");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unlisted path 'logs' is not absolute"), "{stderr}");
}

#[test]
fn no_provider_key_refuses_before_anything_starts() {
    let mut command = Command::new(BIN);
    command
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env("TMPDIR", scratch_dir("tmp"));
    let output = command.output().expect("run the binary with no provider");

    assert!(
        !output.status.success(),
        "a kernel with no model must not start: {:?}",
        output.status
    );
    assert!(
        output.stdout.is_empty(),
        "stdout is the ACP wire and must stay empty on a refusal: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for var in ["DEEPSEEK_API_KEY", "ANTHROPIC_API_KEY", "OPENAI_API_KEY"] {
        assert!(
            stderr.contains(var),
            "the refusal names what to set; {var} missing from:\n{stderr}"
        );
    }
}

#[test]
fn an_unknown_provider_is_refused_on_stderr() {
    let output = Command::new(BIN)
        .arg("--backend-kind")
        .arg("groq")
        .output()
        .expect("run the binary with an unknown provider");
    assert!(!output.status.success(), "an unknown provider is refused");
    assert!(
        output.stdout.is_empty(),
        "stdout stays empty: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for known in ["anthropic", "deepseek", "openai"] {
        assert!(
            stderr.contains(known),
            "the refusal lists the providers it does know; {known} missing from:\n{stderr}"
        );
    }
}

#[test]
fn a_named_gate_policy_lands_in_this_kernels_config() {
    let state = scratch_dir("gate-state").join("state");
    let policy = scratch_dir("gate-policy").join("gate.toml");
    let body = "[global]\nallow = [\n  \"ls\",\n]\n";
    std::fs::write(&policy, body).expect("write the gate policy");

    let mut agent = spawn_mock_with(
        "chat",
        &[
            "--state-dir",
            state.to_str().expect("utf-8 state path"),
            "--gate-config",
            policy.to_str().expect("utf-8 policy path"),
        ],
    );
    // The policy is installed once the kernel is up, which `initialize`
    // proves.
    agent.initialize().expect("initialize");

    let installed = state.join("config").join("kernel").join("gate.toml");
    wait_for("the gate policy to be installed", &agent, || {
        installed.is_file()
    });
    assert_eq!(
        std::fs::read_to_string(&installed).expect("read the installed policy"),
        body,
        "the policy is copied verbatim"
    );

    let status = close_stdin_and_wait(&mut agent, Duration::from_secs(60));
    assert_eq!(status.code(), Some(0));
    assert!(state.is_dir(), "a named state directory survives the exit");
}

/// A minimal rc overlay: `coder/create/S00-stance.kai` reads its companion
/// Markdown the same way `contrib/bench/rc-variants/coder-driven` does, and
/// the companion carries `marker` as its whole body. The shipped
/// `coder/create/S00-stance.kai` is already a regular file (not a symlink),
/// so this overlay exercises the plain-replace path; the symlink-unlink path
/// is covered in `state::tests`.
fn write_marker_overlay(dir: &Path, marker: &str) {
    let coder_dir = dir.join("coder").join("create");
    std::fs::create_dir_all(&coder_dir).expect("create the overlay's coder dir");
    std::fs::write(dir.join("README.md"), "test fixture, not applied\n")
        .expect("write the overlay README");
    std::fs::write(
        coder_dir.join("S00-stance.kai"),
        "set -e\nkj block create --role system --kind text --content-type text/markdown < \"$(dirname \"$0\")/$(basename \"$0\" .kai).md\"\n",
    )
    .expect("write the overlay stance script");
    std::fs::write(coder_dir.join("S00-stance.md"), format!("{marker}\n"))
        .expect("write the overlay stance companion");
}

/// The overlay's marker reaches the coder context's durable system
/// instructions. Observable: reopen the state dir's `kernel.db` after the
/// binary exits (stdin EOF checkpoints it) and read the context's blocks
/// back through the same `BlockStore::load_from_db` + `block_snapshots` path
/// the kernel itself uses — the ACP `sessionId` IS the context id's hex form
/// (`kaijutsu_acp::rank::session_id_of`), so no extra lookup is needed to
/// find which context to read.
#[test]
fn an_rc_overlay_marker_reaches_the_contexts_system_instructions() {
    let state = scratch_dir("rc-overlay-state").join("state");
    let overlay = scratch_dir("rc-overlay-variant");
    let marker = "MARKER-rc-overlay-test-3f9c7a21-unique-instruction-sentence";
    write_marker_overlay(&overlay, marker);

    let mut agent = spawn_mock_with(
        "chat",
        &[
            "--state-dir",
            state.to_str().expect("utf-8 state path"),
            "--rc-overlay",
            overlay.to_str().expect("utf-8 overlay path"),
        ],
    );
    agent.initialize().expect("initialize");
    let cwd = scratch_dir("rc-overlay-cwd");
    // `session/new` runs the create lifecycle synchronously, so the
    // instructions are already durable blocks once this returns; no prompt
    // is needed.
    let session = agent.new_session(&cwd).expect("session/new");

    let status = close_stdin_and_wait(&mut agent, Duration::from_secs(60));
    assert_eq!(status.code(), Some(0), "a clean exit checkpoints the db\n--- stderr ---\n{}", agent.stderr());

    let context_id = kaijutsu_types::ContextId::parse(&session)
        .unwrap_or_else(|e| panic!("session id {session:?} is not a context id: {e}"));
    let db = Arc::new(parking_lot::Mutex::new(
        kaijutsu_kernel::kernel_db::KernelDb::open(state.join("kernel.db"))
            .expect("reopen the kernel db the binary just closed"),
    ));
    let blocks = kaijutsu_kernel::block_store::shared_block_store_with_db(
        db,
        kaijutsu_types::WorkspaceId::new(),
        kaijutsu_types::PrincipalId::system(),
    );
    blocks.load_from_db().expect("load documents from the reopened db");
    let snapshots = blocks
        .block_snapshots(context_id)
        .unwrap_or_else(|e| panic!("read blocks for context {context_id}: {e}"));

    let marker_block = snapshots
        .iter()
        .find(|b| b.content.contains(marker))
        .unwrap_or_else(|| {
            let bodies: Vec<&str> = snapshots.iter().map(|b| b.content.as_str()).collect();
            panic!("no block carried the overlay marker {marker:?}; saw {bodies:?}")
        });
    assert_eq!(
        marker_block.role,
        kaijutsu_types::Role::System,
        "the overlay's stance script authors a system block, same as the shipped one"
    );
}

/// The benchmark adapter opens egress with an overlay create script that
/// runs `kj context set . --egress-allow '*'`. On this binary's real path
/// the rc shell acts as the root character `solo`, which created the
/// session's coder context and is its lineage root, so the change is
/// allowed and lands on the context. Observable: reopen `kernel.db` after
/// exit and read the context's egress rows.
#[test]
fn an_rc_overlay_create_script_opens_the_sessions_egress() {
    let state = scratch_dir("rc-egress-state").join("state");
    let overlay = scratch_dir("rc-egress-variant");
    let create = overlay.join("coder").join("create");
    std::fs::create_dir_all(&create).expect("create the overlay's coder dir");
    std::fs::write(create.join("S50-egress.kai"), "set -e\nkj context set . --egress-allow '*'\n")
        .expect("write the overlay egress script");

    let mut agent = spawn_mock_with(
        "chat",
        &[
            "--state-dir",
            state.to_str().expect("utf-8 state path"),
            "--rc-overlay",
            overlay.to_str().expect("utf-8 overlay path"),
        ],
    );
    agent.initialize().expect("initialize");
    let session = agent.new_session(&scratch_dir("rc-egress-cwd")).expect("session/new");

    let status = close_stdin_and_wait(&mut agent, Duration::from_secs(60));
    assert_eq!(status.code(), Some(0), "a clean exit checkpoints the db\n--- stderr ---\n{}", agent.stderr());

    let context_id = kaijutsu_types::ContextId::parse(&session)
        .unwrap_or_else(|e| panic!("session id {session:?} is not a context id: {e}"));
    let db = kaijutsu_kernel::kernel_db::KernelDb::open(state.join("kernel.db"))
        .expect("reopen the kernel db the binary just closed");
    assert_eq!(
        db.list_context_egress(context_id).expect("read the egress rows"),
        vec!["*".to_string()],
        "the overlay's create script must open egress\n--- stderr ---\n{}",
        agent.stderr()
    );
}

/// `session/new` creates the coder context in the session cwd, so its create
/// lifecycle already sees that cwd: the shipped orientation script describes
/// the session directory, not the kernel's own.
#[test]
fn the_create_lifecycle_runs_in_the_session_cwd() {
    let state = scratch_dir("orient-state").join("state");
    let mut agent = spawn_mock_with("chat", &["--state-dir", state.to_str().expect("utf-8 state path")]);
    agent.initialize().expect("initialize");
    let cwd = scratch_dir("orient-cwd");
    std::fs::write(cwd.join("orient-marker-file.txt"), "x\n").expect("write the marker file");
    let session = agent.new_session(&cwd).expect("session/new");

    let status = close_stdin_and_wait(&mut agent, Duration::from_secs(60));
    assert_eq!(status.code(), Some(0), "a clean exit checkpoints the db\n--- stderr ---\n{}", agent.stderr());

    let context_id = kaijutsu_types::ContextId::parse(&session)
        .unwrap_or_else(|e| panic!("session id {session:?} is not a context id: {e}"));
    let db = Arc::new(parking_lot::Mutex::new(
        kaijutsu_kernel::kernel_db::KernelDb::open(state.join("kernel.db"))
            .expect("reopen the kernel db the binary just closed"),
    ));
    let blocks = kaijutsu_kernel::block_store::shared_block_store_with_db(
        db,
        kaijutsu_types::WorkspaceId::new(),
        kaijutsu_types::PrincipalId::system(),
    );
    blocks.load_from_db().expect("load documents from the reopened db");
    let snapshots = blocks
        .block_snapshots(context_id)
        .unwrap_or_else(|e| panic!("read blocks for context {context_id}: {e}"));
    let orientation = snapshots
        .iter()
        .find(|b| b.content.starts_with("Orientation of the working directory"))
        .unwrap_or_else(|| {
            let bodies: Vec<&str> = snapshots.iter().map(|b| b.content.as_str()).collect();
            panic!("no orientation block; saw {bodies:#?}")
        });
    assert!(
        orientation.content.contains(&format!("cwd: {}", cwd.display()))
            && orientation.content.contains("orient-marker-file.txt"),
        "orientation must describe the session cwd:\n{}",
        orientation.content
    );
}

/// A session cwd the kernel host does not have is the client's mistake:
/// `session/new` answers `invalid_params` naming the cwd, not `internal`,
/// and a later session in a real directory still opens.
#[test]
fn a_session_cwd_missing_on_the_kernel_host_is_invalid_params() {
    let mut agent = spawn_mock("chat");
    agent.initialize().expect("initialize");
    let missing = scratch_dir("missing-session-cwd").join("not-here");
    let error = agent.open_session(&missing).expect_err("a missing cwd must refuse session/new");
    let message = format!("{error:#}");
    assert!(message.contains("-32602"), "expected invalid_params, not internal: {message}");
    assert!(message.contains(missing.to_str().expect("utf-8 path")), "the error names the cwd: {message}");

    let cwd = scratch_dir("present-session-cwd");
    agent.new_session(&cwd).expect("a real directory opens a session");
}

#[test]
fn an_unusable_rc_overlay_refuses_the_boot() {
    let missing = scratch_dir("rc-overlay-refusals").join("missing");
    let plain_file = scratch_dir("rc-overlay-refusals").join("a-file");
    std::fs::write(&plain_file, "not a directory\n").expect("write a plain file");
    let readme_only = scratch_dir("rc-overlay-refusals").join("readme-only");
    std::fs::create_dir_all(&readme_only).expect("create the readme-only overlay");
    std::fs::write(readme_only.join("README.md"), "docs only\n").expect("write the readme");
    let typo = scratch_dir("rc-overlay-refusals").join("typo");
    std::fs::create_dir_all(typo.join("coderr").join("create")).expect("create the typo'd dir");
    std::fs::write(typo.join("coderr").join("create").join("S00-base.kai"), "# typo\n")
        .expect("write the typo'd file");

    let cases: [(&str, &std::path::Path); 4] = [
        ("does not exist", &missing),
        ("is not a directory", &plain_file),
        ("nothing to apply", &readme_only),
        ("no seeded directory", &typo),
    ];
    for (reason, overlay) in cases {
        let output = Command::new(BIN)
            .arg("--backend-kind")
            .arg("mock")
            .arg("--model")
            .arg("solo-mock")
            .arg("--rc-overlay")
            .arg(overlay)
            .env("TMPDIR", scratch_dir("tmp"))
            .output()
            .unwrap_or_else(|e| panic!("run --rc-overlay {}: {e}", overlay.display()));
        assert!(
            !output.status.success(),
            "--rc-overlay {} must refuse the boot, got {:?}",
            overlay.display(),
            output.status
        );
        assert!(
            output.stdout.is_empty(),
            "stdout stays empty on a refusal: {:?}",
            String::from_utf8_lossy(&output.stdout)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(reason),
            "--rc-overlay {} must say why ({reason}):\n{stderr}",
            overlay.display()
        );
    }
}

/// The line the binary prints once the ACP bridge is up. Every exit test
/// waits for it, so a signal always lands on a fully-started process.
const SERVING: &str = "serving ACP v1 on stdio";

#[test]
fn a_signal_removes_the_temp_state_before_exiting() {
    for sig in [libc::SIGTERM, libc::SIGINT] {
        let mut agent = spawn_mock("chat");
        wait_for_stderr(&agent, SERVING, Duration::from_secs(60));
        let state = state_dir_from(&agent.stderr());
        assert!(state.is_dir(), "{} exists while serving", state.display());

        signal(&agent, sig);
        let status = wait_for_exit(&mut agent, "the signal", Duration::from_secs(30));
        assert!(
            status.code().is_some(),
            "signal {sig}: the agent exits rather than dying on the signal: {status:?}"
        );
        assert_eq!(
            agent.stdout_bytes(),
            0,
            "signal {sig}: nothing was asked, so stdout must have stayed empty"
        );
        wait_for("the temp state directory to be removed", &agent, || {
            !state.exists()
        });
    }
}

#[test]
fn a_signal_leaves_a_named_state_directory_alone() {
    let state = scratch_dir("signal-state").join("state");
    let mut agent = spawn_mock_with(
        "chat",
        &["--state-dir", state.to_str().expect("utf-8 state path")],
    );
    wait_for_stderr(&agent, SERVING, Duration::from_secs(60));

    signal(&agent, libc::SIGTERM);
    wait_for_exit(&mut agent, "the signal", Duration::from_secs(30));
    assert!(
        state.join("kernel.db").is_file(),
        "a named state directory is the operator's, never ours to remove"
    );
}

/// The kernel failing while it serves: the process must say so, exit
/// non-zero, and still take its temp state with it.
#[test]
fn a_kernel_that_fails_while_serving_exits_and_cleans_up() {
    let mut agent = spawn_mock_with("chat", &["--fail-kernel-after-serving"]);
    wait_for_stderr(&agent, SERVING, Duration::from_secs(60));
    let state = state_dir_from(&agent.stderr());

    let status = wait_for_exit(&mut agent, "the kernel failure", Duration::from_secs(30));
    assert_eq!(
        status.code(),
        Some(1),
        "a dead kernel is a failed run\n--- stderr ---\n{}",
        agent.stderr()
    );
    assert!(
        agent.stderr().contains("the kernel failed"),
        "the reason reaches stderr:\n{}",
        agent.stderr()
    );
    wait_for("the temp state directory to be removed", &agent, || {
        !state.exists()
    });
}

/// A panic on the kernel thread is the same kind of event as a failure: it
/// must not leave the ACP client waiting on a kernel that no longer exists.
#[test]
fn a_kernel_panic_while_serving_exits_and_cleans_up() {
    let mut agent = spawn_mock_with("chat", &["--panic-kernel-after-serving"]);
    wait_for_stderr(&agent, SERVING, Duration::from_secs(60));
    let state = state_dir_from(&agent.stderr());

    let status = wait_for_exit(&mut agent, "the kernel panic", Duration::from_secs(30));
    assert_eq!(
        status.code(),
        Some(1),
        "a panicked kernel is a failed run\n--- stderr ---\n{}",
        agent.stderr()
    );
    assert!(
        agent.stderr().contains("the kernel panicked"),
        "the panic is named on stderr:\n{}",
        agent.stderr()
    );
    wait_for("the temp state directory to be removed", &agent, || {
        !state.exists()
    });
}

#[test]
fn a_state_dir_inside_the_operators_kaijutsu_refuses() {
    let home = scratch_dir("operator-home");
    let data = home.join("data");
    let config = home.join("config");
    for inside in [data.join("kaijutsu").join("kernel"), config.join("kaijutsu")] {
        let output = Command::new(BIN)
            .arg("--backend-kind")
            .arg("mock")
            .arg("--model")
            .arg("solo-mock")
            .arg("--state-dir")
            .arg(&inside)
            .env("XDG_DATA_HOME", &data)
            .env("XDG_CONFIG_HOME", &config)
            .env("TMPDIR", scratch_dir("tmp"))
            .output()
            .expect("run the binary against the operator's own tree");
        assert!(
            !output.status.success(),
            "{} is the operator's kernel, not a solo one",
            inside.display()
        );
        assert!(output.stdout.is_empty(), "stdout stays empty on a refusal");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&inside.display().to_string()),
            "the refusal names the directory:\n{stderr}"
        );
        assert!(
            !inside.exists(),
            "a refused state directory is not created: {}",
            inside.display()
        );
    }
}

/// A model-spawned process running as the same uid as this agent must not
/// be able to read the provider API key out of `/proc/<pid>/environ` — see
/// docs/solo-acp.md, "The key and /proc". This proves it from the outside,
/// the way any other same-uid process on the host would try: reading
/// `/proc/<agent pid>/environ` once the agent is up must fail with
/// permission denied, never succeed and hand back a sentinel we set in this
/// process's own environment when we spawned it.
#[test]
fn proc_environ_is_unreadable_to_same_uid_readers() {
    // SAFETY: geteuid takes no arguments and only reads this process's own
    // credentials.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!(
            "skipping proc_environ_is_unreadable_to_same_uid_readers: this test \
             process runs as root, which can always read /proc/<pid>/environ \
             regardless of PR_SET_DUMPABLE"
        );
        return;
    }

    let sentinel = format!("SOLO_ACP_PROC_ENVIRON_SENTINEL_{}", std::process::id());
    let agent = spawn(mock_command("chat").env(&sentinel, "leaked-if-this-is-readable"));
    wait_for_stderr(&agent, SERVING, Duration::from_secs(60));

    let pid = agent.pid();
    let environ_path = format!("/proc/{pid}/environ");
    match std::fs::read(&environ_path) {
        Err(e) => {
            assert_eq!(
                e.kind(),
                std::io::ErrorKind::PermissionDenied,
                "{environ_path} must be unreadable to a same-uid process with EACCES, got {e}"
            );
        }
        Ok(bytes) => {
            let leaked = String::from_utf8_lossy(&bytes).contains(&sentinel);
            panic!(
                "{environ_path} was readable by a same-uid process (sentinel present: \
                 {leaked}); the PR_SET_DUMPABLE mitigation is missing or not taking effect"
            );
        }
    }
}

/// `--max-tokens` writes into `llm_defaults.max_tokens`
/// (`kaijutsu_kernel::kernel_db::LlmDefaultsRow`), a plain DB row rather than
/// something the ACP wire reports — so this reads it back through the
/// library the kernel itself uses, from a state directory the binary was
/// told to keep.
#[test]
fn max_tokens_overrides_the_written_defaults_row() {
    let state = scratch_dir("max-tokens-state").join("state");
    let mut agent = spawn_mock_with(
        "chat",
        &["--state-dir", state.to_str().expect("utf-8 state path"), "--max-tokens", "4096"],
    );
    agent.initialize().expect("initialize");

    let status = close_stdin_and_wait(&mut agent, Duration::from_secs(60));
    assert_eq!(status.code(), Some(0));

    let db = kaijutsu_kernel::kernel_db::KernelDb::open(state.join("kernel.db"))
        .expect("reopen the kernel db the binary just closed");
    let defaults = db
        .get_llm_defaults()
        .expect("read the model defaults")
        .expect("the binary wrote a defaults row");
    assert_eq!(
        defaults.max_tokens,
        Some(4096),
        "--max-tokens 4096 must land in the written defaults row"
    );
}

/// Without the flag, the factory ceiling from `seed_backends` stands — the
/// same default `docs/solo-acp.md` documents.
#[test]
fn without_max_tokens_the_factory_ceiling_stands_in_the_written_defaults_row() {
    let state = scratch_dir("max-tokens-default-state").join("state");
    let mut agent = spawn_mock_with(
        "chat",
        &["--state-dir", state.to_str().expect("utf-8 state path")],
    );
    agent.initialize().expect("initialize");

    let status = close_stdin_and_wait(&mut agent, Duration::from_secs(60));
    assert_eq!(status.code(), Some(0));

    let db = kaijutsu_kernel::kernel_db::KernelDb::open(state.join("kernel.db"))
        .expect("reopen the kernel db the binary just closed");
    let defaults = db
        .get_llm_defaults()
        .expect("read the model defaults")
        .expect("the binary wrote a defaults row");
    assert_eq!(defaults.max_tokens, Some(16384), "the factory ceiling stands unless asked");
}

#[test]
fn stdin_eof_exits_clean_and_removes_the_temp_state() {
    let mut agent = spawn_mock("chat");
    agent.initialize().expect("initialize");

    let state = state_dir_from(&agent.stderr());
    assert!(state.is_dir(), "{} should exist while serving", state.display());

    let status = close_stdin_and_wait(&mut agent, Duration::from_secs(60));
    assert_eq!(
        status.code(),
        Some(0),
        "a client disconnect is a clean exit\n--- stderr ---\n{}",
        agent.stderr()
    );
    wait_for("the temp state directory to be removed", &agent, || {
        !state.exists()
    });
}

/// A session at `/` opens, and the client is told the cwd is almost always
/// a mistake. A session in a work tree is not.
#[test]
fn a_session_at_the_root_opens_with_a_warning() {
    let cwd = scratch_dir("root-warning-cwd");
    let mut agent = spawn_mock("chat");
    agent.initialize().expect("initialize");
    let warning = "warning: this session's cwd is /, which is almost always a mistake; \
                   start the client in your work tree, or `cd DIR` in the shell";

    agent.new_session(&cwd).expect("session/new in a work tree");
    assert!(!agent_text(&agent).contains(warning), "a work tree is not warned about: {:?}", agent_text(&agent));

    agent.new_session(Path::new("/")).expect("session/new at / is accepted");
    assert!(
        agent_text(&agent).contains(warning),
        "the client is told; saw {:?}\n--- stderr ---\n{}",
        agent_text(&agent),
        agent.stderr()
    );
}
