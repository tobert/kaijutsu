//! The podman commands contained scenarios run with.
//!
//! A contained agent runs as `podman run -i --rm --network=none
//! --pids-limit=512` in the fleet image (`contrib/Containerfile.fleet`). It
//! sees the agent binary (read-only), the scenario's fleet files — mock
//! script, gate policy, rc overlay — (read-only), and the workspace, its only
//! writable path. A run that needs a council or a model API also mounts the
//! relay's socket directory (read-only) and, for a model API, the key file
//! (read-only); see [`Reach`]. Script verifiers run in a fresh container of
//! the same image over the same workspace, with no network. Nothing here runs
//! a scenario's script on the host.

use std::fmt;
use std::net::Ipv4Addr;
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

/// Where the relay's socket directory is mounted, read-only.
pub const RELAY: &str = "/run/fleet-net";
/// Where the model API key file is mounted, read-only.
pub const KEY: &str = "/run/fleet-key";

/// A TCP address a contained agent may reach through the relay
/// (`crate::relay`), named as the agent names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

impl Endpoint {
    /// The host and port of an `http://` or `https://` URL. The port
    /// defaults to 80 or 443. A contained agent reaches the endpoint by this
    /// name, so the host must be a name or a 127.0.0.0/8 address; any other
    /// address literal is refused.
    pub fn from_url(url: &str) -> Result<Self> {
        let (rest, default_port) = if let Some(rest) = url.strip_prefix("http://") {
            (rest, 80)
        } else if let Some(rest) = url.strip_prefix("https://") {
            (rest, 443)
        } else {
            bail!("{url:?} must start with http:// or https://");
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        if authority.contains('@') || authority.starts_with('[') {
            bail!("{url:?}: name the host plainly, with no user and no IPv6 address");
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => {
                (host, port.parse::<u16>().with_context(|| format!("{url:?}: the port {port:?} is not a number"))?)
            }
            None => (authority, default_port),
        };
        if host.is_empty() {
            bail!("{url:?} names no host");
        }
        if let Ok(ip) = host.parse::<Ipv4Addr>()
            && !ip.is_loopback()
        {
            bail!(
                "{url:?}: a contained agent reaches an endpoint by a name the container maps to loopback, \
                 so name the host instead of giving its address {ip}"
            );
        }
        Ok(Self { host: host.to_string(), port })
    }

    /// The loopback address the endpoint numbered `n` gets inside the
    /// container. A loopback address names itself; a host name maps to
    /// `127.0.2.<n+1>` through `--add-host`.
    pub fn inside(&self, n: usize) -> Result<Ipv4Addr> {
        if let Ok(ip) = self.host.parse::<Ipv4Addr>() {
            return Ok(ip);
        }
        let last = u8::try_from(n + 1).ok().filter(|l| *l < 255).context("a contained run reaches at most 254 named hosts")?;
        Ok(Ipv4Addr::new(127, 0, 2, last))
    }

    fn is_named(&self) -> bool {
        self.host.parse::<Ipv4Addr>().is_err()
    }
}

/// What a contained agent may reach beyond its mounts. The default reaches
/// nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct Reach<'a> {
    /// The relay's socket directory and its endpoints, in socket order.
    pub relay: Option<(&'a Path, &'a [Endpoint])>,
    /// A model API key file, mounted read-only and read into the named
    /// environment variable as the agent starts.
    pub key: Option<(&'a Path, &'a str)>,
}

impl Reach<'_> {
    fn is_empty(&self) -> bool {
        self.relay.is_none_or(|(_, endpoints)| endpoints.is_empty()) && self.key.is_none()
    }
}

/// The command that starts `agent` in a container named `name`. `args` are
/// the agent's own arguments, with paths as the container sees them. With a
/// non-empty `reach`, `bash` starts first: it runs a `socat` per endpoint,
/// waits for each to listen, reads the key file into its variable, then
/// replaces itself with the agent ([`start_script`]).
pub fn agent_command(
    agent: &Path,
    workspace: &Path,
    fleet: &Path,
    name: &str,
    args: &[&str],
    reach: Reach<'_>,
) -> Result<AgentCommand> {
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
        .arg("RUST_LOG=info");
    if let Some((dir, endpoints)) = reach.relay {
        command = command.arg("-v").arg(format!("{}:{RELAY}:ro", dir.display()));
        for (n, endpoint) in endpoints.iter().enumerate() {
            if endpoint.is_named() {
                command = command.arg("--add-host").arg(format!("{}:{}", endpoint.host, endpoint.inside(n)?));
            }
        }
    }
    if let Some((file, _)) = reach.key {
        command = command.arg("-v").arg(format!("{}:{KEY}:ro", file.display()));
    }
    command = command.arg(IMAGE);
    command = if reach.is_empty() {
        command.arg(AGENT)
    } else {
        command.arg("bash").arg("-c").arg(start_script(reach)?).arg("fleet-agent")
    };
    for arg in args {
        command = command.arg(*arg);
    }
    Ok(command)
}

/// The `bash` script that starts a contained agent with `reach`: one
/// `socat` per endpoint, from its loopback address inside the container to
/// its relay socket, a wait until each listens, the key file read into its
/// variable, then `exec` of the agent with the script's arguments. The key
/// never appears in the script.
pub fn start_script(reach: Reach<'_>) -> Result<String> {
    let mut script = String::from(
        "set -euo pipefail\n\
         listening() {\n\
         \x20 for _ in $(seq 200); do\n\
         \x20   grep -q \" $1 00000000:0000 0A \" /proc/net/tcp && return 0\n\
         \x20   sleep 0.05\n\
         \x20 done\n\
         \x20 echo \"acp-fleet: the relay to $2 never listened in the container\" >&2\n\
         \x20 exit 1\n\
         }\n",
    );
    if let Some((_, endpoints)) = reach.relay {
        for (n, endpoint) in endpoints.iter().enumerate() {
            let ip = endpoint.inside(n)?;
            let socket = crate::relay::Relay::socket_name(n);
            script.push_str(&format!(
                "socat TCP-LISTEN:{port},bind={ip},fork,reuseaddr UNIX-CONNECT:{RELAY}/{socket} >/dev/null &\n\
                 listening {hex} {endpoint}\n",
                port = endpoint.port,
                hex = proc_net_tcp(ip, endpoint.port),
            ));
        }
    }
    if let Some((_, var)) = reach.key {
        if var.is_empty() || !var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            bail!("the key variable {var:?} must be letters, digits, and _");
        }
        script.push_str(&format!("key=\"$(cat {KEY})\"\nexport {var}=\"$key\"\nunset key\n"));
    }
    script.push_str(&format!("exec {AGENT} \"$@\"\n"));
    Ok(script)
}

/// `ip:port` as `/proc/net/tcp` prints a local address: the address's bytes
/// in host (little-endian) order, then the port, both in upper-case hex.
fn proc_net_tcp(ip: Ipv4Addr, port: u16) -> String {
    let [a, b, c, d] = ip.octets();
    format!("{d:02X}{c:02X}{b:02X}{a:02X}:{port:04X}")
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
        let command = agent_command(Path::new("/a"), Path::new("/w"), Path::new("/f"), "n", &[], Reach::default()).unwrap();
        let args = args(&command);
        let image = args.iter().position(|a| a == IMAGE).expect("the image is named");
        assert!(args[..image].contains(&format!("--pids-limit={PIDS_LIMIT}")), "{args:?}");
    }

    #[test]
    fn a_container_that_reaches_nothing_runs_the_agent_directly() {
        let command = agent_command(Path::new("/a"), Path::new("/w"), Path::new("/f"), "n", &["--x"], Reach::default()).unwrap();
        let args = args(&command);
        let image = args.iter().position(|a| a == IMAGE).unwrap();
        assert_eq!(args[image + 1..], [AGENT.to_string(), "--x".to_string()]);
        assert!(args.contains(&"--network=none".to_string()), "{args:?}");
        assert!(!args.iter().any(|a| a.contains(RELAY) || a.contains(KEY) || a == "--add-host"), "{args:?}");
    }

    #[test]
    fn a_container_reaches_its_endpoints_through_loopback_and_the_relay() {
        let endpoints = [
            Endpoint { host: "127.0.0.1".into(), port: 39001 },
            Endpoint { host: "api.deepseek.com".into(), port: 443 },
        ];
        let reach = Reach { relay: Some((Path::new("/scratch/net-1"), &endpoints)), key: Some((Path::new("/keys/k"), "KEY_VAR")) };
        let command = agent_command(Path::new("/a"), Path::new("/w"), Path::new("/f"), "n", &["--x"], reach).unwrap();
        let args = args(&command);
        let image = args.iter().position(|a| a == IMAGE).unwrap();
        let before = &args[..image];
        assert!(before.contains(&"--network=none".to_string()), "the container keeps no network of its own: {args:?}");
        assert!(before.windows(2).any(|w| w[0] == "--add-host" && w[1] == "api.deepseek.com:127.0.2.2"), "{args:?}");
        assert_eq!(before.iter().filter(|a| *a == "--add-host").count(), 1, "a loopback address needs no host entry: {args:?}");
        assert!(before.contains(&format!("/scratch/net-1:{RELAY}:ro")), "{args:?}");
        assert!(before.contains(&format!("/keys/k:{KEY}:ro")), "{args:?}");
        assert_eq!(args[image + 1..image + 3], ["bash".to_string(), "-c".to_string()]);
        assert_eq!(args[image + 4..], ["fleet-agent".to_string(), "--x".to_string()]);
        let script = &args[image + 3];
        assert!(script.contains(&format!("socat TCP-LISTEN:39001,bind=127.0.0.1,fork,reuseaddr UNIX-CONNECT:{RELAY}/0.sock")), "{script}");
        assert!(script.contains(&format!("socat TCP-LISTEN:443,bind=127.0.2.2,fork,reuseaddr UNIX-CONNECT:{RELAY}/1.sock")), "{script}");
        assert!(script.contains("listening 0100007F:9859 127.0.0.1:39001"), "{script}");
        assert!(script.contains("listening 0202007F:01BB api.deepseek.com:443"), "{script}");
        assert!(script.contains(&format!("key=\"$(cat {KEY})\"\nexport KEY_VAR=\"$key\"")), "{script}");
        assert!(script.ends_with(&format!("exec {AGENT} \"$@\"\n")), "{script}");
    }

    #[test]
    fn a_key_variable_must_be_a_plain_name() {
        let reach = Reach { relay: None, key: Some((Path::new("/k"), "X; rm -rf /")) };
        assert!(format!("{:#}", start_script(reach).unwrap_err()).contains("letters, digits"));
    }

    #[test]
    fn an_endpoint_comes_from_its_url() {
        let at = |url: &str| Endpoint::from_url(url).map(|e| e.to_string()).map_err(|e| format!("{e:#}"));
        assert_eq!(at("http://zorak:8090").unwrap(), "zorak:8090");
        assert_eq!(at("http://127.0.0.1:41234/council").unwrap(), "127.0.0.1:41234");
        assert_eq!(at("https://api.deepseek.com").unwrap(), "api.deepseek.com:443");
        assert_eq!(at("http://zorak").unwrap(), "zorak:80");
        assert!(at("ftp://zorak").unwrap_err().contains("http://"));
        assert!(at("http://192.168.1.5:8090").unwrap_err().contains("name the host"));
        assert!(at("http://zorak:port").unwrap_err().contains("not a number"));
        assert!(at("http://[::1]:80").unwrap_err().contains("IPv6"));
        assert!(at("http://:80").unwrap_err().contains("no host"));
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
