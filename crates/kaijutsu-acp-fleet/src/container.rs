//! The podman commands contained scenarios run with.
//!
//! A contained agent runs as `podman run -i --rm --network=none` in the
//! fleet image (`contrib/Containerfile.fleet`), with the limits and
//! hardening every fleet container gets: memory, CPU, and process limits, a
//! read-only root, no capabilities, no privilege gain, and a label naming
//! its runner ([`hardening`]). It sees the agent binary (read-only), the
//! scenario's fleet files — mock script, gate policy, rc overlay —
//! (read-only), and the workspace, its only writable path besides tmpfs. A run that needs a council or a model API also mounts the
//! relay's socket directory (read-only) and, for a model API, the key file
//! (read-only); see [`Reach`]. Script verifiers run in a fresh container of
//! the same image over the same workspace, with no network and the same
//! hardening. Nothing here runs a scenario's script on the host.

use std::fmt;
use std::net::Ipv4Addr;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// The memory limit of a fleet container, with no swap beyond it. A
/// contained agent with kaish and git uses a few hundred MB.
pub const MEMORY: &str = "4g";
/// The CPU limit of a fleet container, in cores.
pub const CPUS: &str = "4";
/// The label naming the runner that started a fleet container: its pid
/// and start time ([`runner_label`]). [`preflight`] removes containers whose
/// runner no longer runs.
pub const LABEL: &str = "kaijutsu.acp-fleet.runner";

/// The limits and hardening every fleet container runs with: memory, CPU,
/// and process limits, a read-only root filesystem (podman mounts tmpfs at
/// `/tmp`, `/var/tmp`, and `/run`), no capabilities, no privilege gain
/// through setuid, and this runner's [`LABEL`]. Nothing a contained agent,
/// `socat`, or a verifier runs needs a capability; `socat` binds a port
/// below 1024 through a network-namespace sysctl instead
/// ([`agent_command`]).
fn hardening() -> Vec<String> {
    vec![
        format!("--memory={MEMORY}"),
        format!("--memory-swap={MEMORY}"),
        format!("--cpus={CPUS}"),
        format!("--pids-limit={PIDS_LIMIT}"),
        "--security-opt=no-new-privileges".to_string(),
        "--cap-drop=ALL".to_string(),
        "--read-only".to_string(),
        format!("--label={LABEL}={}", runner_label()),
    ]
}

/// This runner's label value: `<pid>-<start>`, where `<start>` is the
/// process start time from `/proc/<pid>/stat`, so a reused pid does not
/// read as this runner. Panics when `/proc` cannot say when this process
/// started: a label that matched no runner would let another runner's sweep
/// remove this one's containers.
pub fn runner_label() -> String {
    static LABEL_VALUE: OnceLock<String> = OnceLock::new();
    LABEL_VALUE
        .get_or_init(|| {
            let pid = std::process::id();
            let start = process_start(pid).expect("read this process's start time from /proc/<pid>/stat");
            format!("{pid}-{start}")
        })
        .clone()
}

/// The start time of process `pid`, in clock ticks after boot.
fn process_start(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may hold spaces and parentheses; fields resume after
    // the last `)`. starttime is field 22, the 20th after it.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Whether the runner a label value names still runs.
fn runner_alive(label: &str) -> bool {
    let Some((pid, start)) = label.split_once('-') else { return false };
    match (pid.parse::<u32>(), start.parse::<u64>()) {
        (Ok(pid), Ok(start)) => process_start(pid) == Some(start),
        _ => false,
    }
}

/// What [`preflight`] did about containers a stopped runner left behind.
#[derive(Debug, Default, Clone)]
pub struct Sweep {
    /// How many it removed.
    pub removed: usize,
    /// Each one it could not list or remove, with podman's reason.
    pub failed: Vec<String>,
}

/// The ids of the containers in `listing`, `podman ps --format json`
/// output, whose [`LABEL`] names a runner that no longer runs.
fn leftovers(listing: &str, alive: impl Fn(&str) -> bool) -> Result<Vec<String>> {
    let rows: Vec<serde_json::Value> = serde_json::from_str(listing).context("podman ps printed something other than a JSON list")?;
    let mut ids = Vec::new();
    for row in rows {
        let id = row["Id"].as_str().context("a podman ps row has no Id")?;
        if let Some(label) = row["Labels"][LABEL].as_str()
            && !alive(label)
        {
            ids.push(id.to_string());
        }
    }
    Ok(ids)
}

/// Remove every fleet container whose runner no longer runs, such as one a
/// killed runner left behind. A container of a runner still running is
/// left alone.
fn sweep() -> Sweep {
    let mut sweep = Sweep::default();
    let listed = Command::new("podman")
        .args(["ps", "-a", "--filter", &format!("label={LABEL}"), "--format", "json"])
        .stdin(Stdio::null())
        .output();
    let listing = match listed {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        Ok(out) => {
            sweep.failed.push(format!("list leftover fleet containers: {}", String::from_utf8_lossy(&out.stderr).trim()));
            return sweep;
        }
        Err(error) => {
            sweep.failed.push(format!("list leftover fleet containers: {error}"));
            return sweep;
        }
    };
    let ids = match leftovers(&listing, runner_alive) {
        Ok(ids) => ids,
        Err(error) => {
            sweep.failed.push(format!("list leftover fleet containers: {error:#}"));
            return sweep;
        }
    };
    for name in ids {
        match remove(&name) {
            Ok(()) => sweep.removed += 1,
            Err(error) => sweep.failed.push(format!("{error:#}")),
        }
    }
    sweep
}

/// The Containerfile this crate was built with. The image carries a copy at
/// [`IMAGE_CONTAINERFILE`], and [`preflight`] refuses an image whose copy
/// differs.
pub const CONTAINERFILE: &str = include_str!("../../../contrib/Containerfile.fleet");

/// Where the image keeps the copy of the Containerfile it was built from.
pub const IMAGE_CONTAINERFILE: &str = "/opt/kaijutsu/Containerfile.fleet";

/// Fail, naming the build command, unless podman runs and has the image, and
/// the image was built from the current `contrib/Containerfile.fleet`. The
/// answer is computed once per process.
///
/// The first call also removes the containers a stopped runner left behind
/// and returns what it did; later calls return an empty [`Sweep`], so a run
/// reports it once.
pub fn preflight() -> Result<Sweep> {
    static CHECKED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    static SWEPT: OnceLock<Sweep> = OnceLock::new();
    static REPORTED: AtomicBool = AtomicBool::new(false);
    CHECKED.get_or_init(|| check_image().map_err(|e| format!("{e:#}"))).clone().map_err(|e| anyhow!(e))?;
    let swept = SWEPT.get_or_init(sweep);
    Ok(if REPORTED.swap(true, Ordering::SeqCst) { Sweep::default() } else { swept.clone() })
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
    /// name, so the host must be a name or a 127.0.0.0/8 address as four
    /// dotted decimal numbers; any other address, and any host a resolver
    /// would read as a number ([`is_numeric`]), is refused.
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
        if is_numeric(host) && host.parse::<Ipv4Addr>().is_err() {
            bail!(
                "{url:?}: the host {host:?} reads as an IPv4 address in a form other than dotted decimal; \
                 give a name, or a 127.0.0.0/8 address as four dotted decimal numbers"
            );
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

/// Whether `host` reads as an IPv4 address to a resolver that accepts the
/// `inet_aton` forms: its last dot-separated label, after any trailing dot,
/// is a decimal, octal, or `0x` hexadecimal number, as in `2130706433`,
/// `0x7f000001`, `127.1`, or `127.0.0.1.`.
fn is_numeric(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host);
    let last = host.rsplit('.').next().unwrap_or(host);
    let hex = last.strip_prefix("0x").or_else(|| last.strip_prefix("0X"));
    match hex {
        Some(digits) => digits.chars().all(|c| c.is_ascii_hexdigit()),
        None => !last.is_empty() && last.chars().all(|c| c.is_ascii_digit()),
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
        .arg("--network=none");
    for flag in hardening() {
        command = command.arg(flag);
    }
    command = command
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
        // socat binds each endpoint's port, 443 for a model API, with no
        // capability; the sysctl is scoped to the container's own network.
        command = command
            .arg("-v")
            .arg(format!("{}:{RELAY}:ro", dir.display()))
            .arg("--sysctl")
            .arg("net.ipv4.ip_unprivileged_port_start=0");
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
pub fn remove(name: &str) -> Result<()> {
    let out = Command::new("podman")
        .args(["rm", "-f", "--ignore", name])
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run podman rm for the container {name}"))?;
    if !out.status.success() {
        bail!("remove the container {name}: podman rm exited {}: {}", out.status, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// The `podman` arguments that run `script` over `workspace` in a
/// container named `name`.
fn script_args(workspace: &Path, script: &str, name: &str) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = ["run", "--rm", "--init", "--network=none", "--name", name, "-v"].map(Into::into).into();
    args.push(format!("{}:{WORKSPACE}:rw", workspace.display()).into());
    args.extend(hardening().into_iter().map(Into::into));
    args.extend(["-w", WORKSPACE, IMAGE, "bash", "-xeuo", "pipefail", "-c", script].map(Into::into));
    args
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
        .args(script_args(workspace, script, name))
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
            let removed = remove(name);
            let _ = child.kill();
            let _ = child.wait();
            removed?;
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

    /// The limits and hardening every fleet container runs with, before the image.
    fn assert_hardened(args: &[String]) {
        let image = args.iter().position(|a| a == IMAGE).expect("the image is named");
        let before = &args[..image];
        for want in [
            format!("--memory={MEMORY}"),
            format!("--memory-swap={MEMORY}"),
            format!("--cpus={CPUS}"),
            format!("--pids-limit={PIDS_LIMIT}"),
            "--security-opt=no-new-privileges".to_string(),
            "--cap-drop=ALL".to_string(),
            "--read-only".to_string(),
            "--network=none".to_string(),
            format!("--label={LABEL}={}", runner_label()),
        ] {
            assert!(before.contains(&want), "{want} missing: {args:?}");
        }
        assert!(!before.iter().any(|a| a.starts_with("--cap-add")), "a fleet container adds no capability: {args:?}");
    }

    #[test]
    fn the_agent_container_is_limited_and_hardened() {
        let command = agent_command(Path::new("/a"), Path::new("/w"), Path::new("/f"), "n", &[], Reach::default()).unwrap();
        assert_hardened(&args(&command));
        let endpoints = [Endpoint { host: "api.deepseek.com".into(), port: 443 }];
        let reach = Reach { relay: Some((Path::new("/scratch/net-1"), &endpoints)), key: None };
        let command = agent_command(Path::new("/a"), Path::new("/w"), Path::new("/f"), "n", &[], reach).unwrap();
        let args = args(&command);
        assert_hardened(&args);
        assert!(
            args.windows(2).any(|w| w[0] == "--sysctl" && w[1] == "net.ipv4.ip_unprivileged_port_start=0"),
            "socat binds 443 with no capability: {args:?}"
        );
    }

    #[test]
    fn the_script_verifier_container_is_limited_and_hardened() {
        let args: Vec<String> =
            script_args(Path::new("/w"), "true", "n").iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_hardened(&args);
        assert!(args.contains(&format!("/w:{WORKSPACE}:rw")), "{args:?}");
    }

    #[test]
    fn a_runner_label_names_this_process_and_its_start() {
        let label = runner_label();
        let (pid, start) = label.split_once('-').expect("pid-start");
        assert_eq!(pid, std::process::id().to_string());
        assert!(start.parse::<u64>().is_ok(), "{label}");
        assert!(runner_alive(&label), "this runner is alive");
        assert!(!runner_alive(&format!("{pid}-{}", start.parse::<u64>().unwrap() + 1)), "a reused pid is not this runner");
        assert!(!runner_alive("not-a-label"));
    }

    #[test]
    fn only_a_stopped_runners_containers_are_leftovers() {
        let listing = format!(
            r#"[{{"Id": "a", "Labels": {{"{LABEL}": "100-5"}}}}, {{"Id": "b", "Labels": {{"{LABEL}": "200-6"}}}},
                {{"Id": "c", "Labels": {{}}}}, {{"Id": "d", "Labels": null}}]"#
        );
        assert_eq!(leftovers(&listing, |label| label == "100-5").unwrap(), ["b"]);
        assert_eq!(leftovers("[]", |_| true).unwrap(), Vec::<String>::new());
        assert!(leftovers("not json", |_| true).is_err());
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
        for numeric in ["http://0x7f000001:80", "http://2130706433:80", "http://127.1:80", "http://0177.0.0.1:80",
                        "http://127.0.0.1.:80", "http://0x7f.0.0.1:80", "http://1.2.3.04:80"] {
            assert!(at(numeric).unwrap_err().contains("dotted"), "{numeric} is a number, not a name");
        }
        assert_eq!(at("http://zorak.lan:8090").unwrap(), "zorak.lan:8090");
        assert_eq!(at("http://api.deepseek.com").unwrap(), "api.deepseek.com:80");
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
