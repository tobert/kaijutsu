//! The podman commands contained scenarios run with.
//!
//! A contained agent runs as `podman run -i --rm --network=none` in the
//! fleet image (`contrib/Containerfile.fleet`). It sees three mounts and
//! nothing else of the host: the agent binary (read-only), the scenario's
//! fleet files — mock script, gate policy, rc overlay — (read-only), and the
//! workspace, its only writable path. Script verifiers run in a fresh
//! container of the same image over the same workspace. Nothing here runs a
//! scenario's script on the host.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};

use crate::client::AgentCommand;

/// The image contained scenarios run in.
pub const IMAGE: &str = "localhost/kaijutsu-fleet";

/// The command that builds [`IMAGE`], run from the repository root.
pub const BUILD_COMMAND: &str = "podman build -t kaijutsu-fleet -f contrib/Containerfile.fleet contrib";

/// Where the workspace is mounted, read-write, and the session cwd.
pub const WORKSPACE: &str = "/work";
/// Where the scenario's fleet files are mounted, read-only.
pub const FLEET: &str = "/fleet";
/// Where the agent binary is mounted, read-only.
pub const AGENT: &str = "/opt/kaijutsu/kaijutsu-solo-acp";

/// Fail, naming the build command, unless podman runs and has the image.
pub fn preflight() -> Result<()> {
    let status = Command::new("podman")
        .args(["image", "exists", IMAGE])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("run podman, which contained scenarios need; install it, then run `{BUILD_COMMAND}`"))?;
    if !status.success() {
        bail!("the image {IMAGE} is missing; build it from the repository root with `{BUILD_COMMAND}`");
    }
    Ok(())
}

/// The command that starts `agent` in a container named `name`. `args` are
/// the agent's own arguments, with paths as the container sees them.
pub fn agent_command(agent: &Path, workspace: &Path, fleet: &Path, name: &str, args: &[&str]) -> AgentCommand {
    let mut command = AgentCommand::new("podman")
        .arg("run")
        .arg("-i")
        .arg("--rm")
        .arg("--init")
        .arg("--network=none")
        .arg("--name")
        .arg(name)
        .arg("-v")
        .arg(format!("{}:{AGENT}:ro", agent.display()))
        .arg("-v")
        .arg(format!("{}:{FLEET}:ro", fleet.display()))
        .arg("-v")
        .arg(format!("{}:{WORKSPACE}:rw", workspace.display()))
        .arg("-w")
        .arg(WORKSPACE)
        .arg("-e")
        .arg(format!("KJ_MOCK_SCRIPT_DIR={FLEET}/mock"))
        .arg("-e")
        .arg("RUST_LOG=info")
        .arg(IMAGE)
        .arg(AGENT);
    for arg in args {
        command = command.arg(*arg);
    }
    command
}

/// Remove the container `name` if it still exists. A run that ended cleanly
/// has already removed it (`--rm`); this covers a killed podman client.
pub fn remove(name: &str) {
    let _ = Command::new("podman")
        .args(["rm", "-f", "--ignore", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// What a script verifier did.
pub struct ScriptRun {
    pub success: bool,
    pub code: Option<i32>,
    pub output: String,
}

/// Run `script` with `bash -xeuo pipefail` in a fresh container over `workspace`, with no
/// network. Stdout and stderr are returned together.
pub fn run_script(workspace: &Path, script: &str, name: &str, within: Duration) -> Result<ScriptRun> {
    let log = workspace.with_extension("verify.log");
    let file = std::fs::File::create(&log).with_context(|| format!("create {}", log.display()))?;
    let mut child = Command::new("podman")
        .args(["run", "--rm", "--init", "--network=none", "--name", name, "-v"])
        .arg(format!("{}:{WORKSPACE}:rw", workspace.display()))
        .args(["-w", WORKSPACE, IMAGE, "bash", "-xeuo", "pipefail", "-c", script])
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file)
        .spawn()
        .context("start the script verifier's container")?;
    let deadline = Instant::now() + within;
    let status = loop {
        if let Some(status) = child.try_wait().context("poll the script verifier")? {
            break status;
        }
        if Instant::now() > deadline {
            remove(name);
            let _ = child.kill();
            let _ = child.wait();
            bail!("the script verifier did not finish within {within:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let output = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    Ok(ScriptRun { success: status.success(), code: status.code(), output })
}
