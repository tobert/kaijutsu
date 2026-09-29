//! The podman commands contained scenarios run with.
//!
//! A contained agent runs as `podman run -i --rm --network=none
//! --pids-limit=512` in the fleet image (`contrib/Containerfile.fleet`). It sees three mounts and
//! nothing else of the host: the agent binary (read-only), the scenario's
//! fleet files — mock script, gate policy, rc overlay — (read-only), and the
//! workspace, its only writable path. Script verifiers run in a fresh
//! container of the same image over the same workspace. Nothing here runs a
//! scenario's script on the host.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};

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

/// The most processes and threads one container may hold. A contained
/// agent uses about 70 on a 24-core host; the limit stops a runaway command
/// from exhausting the host's.
pub const PIDS_LIMIT: u32 = 512;

/// The Containerfile this crate was built with. The image carries a copy at
/// [`IMAGE_CONTAINERFILE`], and [`preflight`] refuses an image whose copy
/// differs.
pub const CONTAINERFILE: &str = include_str!("../../../contrib/Containerfile.fleet");

/// Where the image keeps the copy of the Containerfile it was built from.
pub const IMAGE_CONTAINERFILE: &str = "/opt/kaijutsu/Containerfile.fleet";

/// Fail, naming the build command, unless podman runs and has the image, and
/// the image was built from the current `contrib/Containerfile.fleet`. The
/// answer is computed once per process.
pub fn preflight() -> Result<()> {
    static CHECKED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    CHECKED.get_or_init(|| check_image().map_err(|e| format!("{e:#}"))).clone().map_err(|e| anyhow!(e))
}

fn check_image() -> Result<()> {
    let status = Command::new("podman")
        .args(["image", "exists", IMAGE])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("run podman, which contained scenarios need; install it, then run `{BUILD_COMMAND}`"))?;
    if !status.success() {
        bail!("the image {IMAGE} is missing; build it from the repository root with `{BUILD_COMMAND}`");
    }
    let copy = Command::new("podman")
        .args(["run", "--rm", "--network=none", IMAGE, "cat", IMAGE_CONTAINERFILE])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("read {IMAGE_CONTAINERFILE} from the image {IMAGE}"))?;
    // An image with no copy predates the check; it is stale too.
    let in_image = if copy.status.success() { String::from_utf8_lossy(&copy.stdout).into_owned() } else { String::new() };
    check_containerfile(&in_image)
}

/// Fail unless `in_image`, the image's copy of its Containerfile, is the one
/// this crate was built with.
fn check_containerfile(in_image: &str) -> Result<()> {
    if in_image != CONTAINERFILE {
        bail!(
            "the image {IMAGE} is stale: it was not built from the current contrib/Containerfile.fleet; \
             rebuild it from the repository root with `{BUILD_COMMAND}`"
        );
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
        .arg(format!("--pids-limit={PIDS_LIMIT}"))
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
        .arg(format!("--pids-limit={PIDS_LIMIT}"))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(command: &AgentCommand) -> Vec<String> {
        command.args.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn the_agent_container_limits_its_process_count() {
        let command = agent_command(Path::new("/a"), Path::new("/w"), Path::new("/f"), "n", &[]);
        let args = args(&command);
        let image = args.iter().position(|a| a == IMAGE).expect("the image is named");
        assert!(args[..image].contains(&format!("--pids-limit={PIDS_LIMIT}")), "{args:?}");
    }

    #[test]
    fn an_image_built_from_this_containerfile_is_current() {
        check_containerfile(CONTAINERFILE).unwrap();
    }

    #[test]
    fn an_image_built_from_another_containerfile_is_stale() {
        let error = format!("{:#}", check_containerfile("FROM somewhere-else\n").unwrap_err());
        assert!(error.contains("stale") && error.contains(BUILD_COMMAND), "{error}");
    }

    #[test]
    fn an_image_with_no_containerfile_copy_is_stale() {
        let error = format!("{:#}", check_containerfile("").unwrap_err());
        assert!(error.contains("stale") && error.contains(BUILD_COMMAND), "{error}");
    }
}
