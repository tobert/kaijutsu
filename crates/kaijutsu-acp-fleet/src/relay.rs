//! The host end of a contained agent's network (`docs/acp-fleet.md`,
//! "Contained mode").
//!
//! A contained agent runs with `--network=none`: its only interface is
//! loopback. For each endpoint the run needs, the relay listens on a Unix
//! socket in a directory the container mounts read-only, and splices each
//! connection to that endpoint's TCP address. Inside the container, `socat`
//! listens on a loopback address and forwards to the socket
//! ([`crate::container::agent_command`]). The container reaches exactly the
//! endpoints listed here and nothing else; TLS runs end to end through the
//! splice.

use std::net::{Shutdown, TcpStream, ToSocketAddrs as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};

use crate::container::Endpoint;

/// The longest Unix socket path Linux accepts, less the terminating NUL.
const SOCKET_PATH_MAX: usize = 107;

/// How long the relay waits to connect to an endpoint.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// What one endpoint's listener has done.
#[derive(Default)]
struct Tally {
    connections: AtomicU64,
    /// Upstream connections that failed, each named.
    errors: Mutex<Vec<String>>,
}

struct Listener {
    endpoint: Endpoint,
    socket: PathBuf,
    tally: Arc<Tally>,
    accept: Option<JoinHandle<()>>,
}

/// Unix sockets that splice to TCP endpoints, alive until dropped. Dropping
/// it removes its directory.
pub struct Relay {
    dir: PathBuf,
    stop: Arc<AtomicBool>,
    listeners: Vec<Listener>,
}

impl Relay {
    /// Listen for `endpoints`, in order, at `<dir>/<n>.sock` in a new
    /// directory under `parent`. [`Relay::socket_name`] gives each name.
    pub fn start(parent: &Path, endpoints: &[Endpoint]) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = parent.join(format!("net-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::SeqCst)));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("remove the stale relay directory {}", dir.display()))?;
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("create the relay directory {}", dir.display()))?;
        let stop = Arc::new(AtomicBool::new(false));
        let mut relay = Self { dir, stop, listeners: Vec::new() };
        for (n, endpoint) in endpoints.iter().enumerate() {
            let socket = relay.dir.join(Self::socket_name(n));
            if socket.as_os_str().len() > SOCKET_PATH_MAX {
                bail!(
                    "the relay socket path {} is longer than {SOCKET_PATH_MAX} bytes, which a Unix socket allows; \
                     use a shorter --scratch",
                    socket.display()
                );
            }
            let listener = UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
            let tally = Arc::new(Tally::default());
            let (target, counts, stopping) = (endpoint.clone(), tally.clone(), relay.stop.clone());
            let accept = std::thread::spawn(move || {
                for client in listener.incoming() {
                    if stopping.load(Ordering::SeqCst) {
                        return;
                    }
                    let Ok(client) = client else { continue };
                    counts.connections.fetch_add(1, Ordering::SeqCst);
                    let (target, counts) = (target.clone(), counts.clone());
                    std::thread::spawn(move || {
                        if let Err(error) = splice(client, &target)
                            && let Ok(mut errors) = counts.errors.lock()
                        {
                            errors.push(format!("{error:#}"));
                        }
                    });
                }
            });
            relay.listeners.push(Listener { endpoint: endpoint.clone(), socket, tally, accept: Some(accept) });
        }
        Ok(relay)
    }

    /// The file name of the socket for the endpoint numbered `n`, from 0.
    pub fn socket_name(n: usize) -> String {
        format!("{n}.sock")
    }

    /// The directory holding the sockets, which the container mounts.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// One line per endpoint: how many connections it carried, and each
    /// upstream connection that failed.
    pub fn report(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for listener in &self.listeners {
            let count = listener.tally.connections.load(Ordering::SeqCst);
            lines.push(format!("relay to {} carried {count} connection(s)", listener.endpoint));
            let errors = listener.tally.errors.lock().map(|e| e.clone()).unwrap_or_default();
            for error in errors {
                lines.push(format!("relay to {}: {error}", listener.endpoint));
            }
        }
        lines
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for listener in &mut self.listeners {
            // Wake the accept loop so it sees the flag.
            let _ = UnixStream::connect(&listener.socket);
            if let Some(accept) = listener.accept.take() {
                let _ = accept.join();
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Connect to `target` and copy bytes both ways until each side closes.
fn splice(client: UnixStream, target: &Endpoint) -> Result<()> {
    let address = (target.host.as_str(), target.port)
        .to_socket_addrs()
        .with_context(|| format!("resolve {target}"))?
        .next()
        .with_context(|| format!("{target} resolves to no address"))?;
    let upstream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).with_context(|| format!("connect to {target}"))?;
    let (mut client_read, mut upstream_write) = (client.try_clone()?, upstream.try_clone()?);
    let outbound = std::thread::spawn(move || {
        let _ = copy(&mut client_read, &mut upstream_write);
        let _ = upstream_write.shutdown(Shutdown::Write);
    });
    let (mut upstream_read, mut client_write) = (upstream, client);
    let _ = copy(&mut upstream_read, &mut client_write);
    let _ = client_write.shutdown(Shutdown::Write);
    let _ = outbound.join();
    Ok(())
}

fn copy(from: &mut impl std::io::Read, to: &mut impl std::io::Write) -> std::io::Result<()> {
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = from.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        to.write_all(&buf[..n])?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    #[test]
    fn a_connection_to_the_socket_reaches_the_endpoint_and_back() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        let echo = std::thread::spawn(move || {
            let (mut sock, _) = server.accept().unwrap();
            let mut got = Vec::new();
            sock.read_to_end(&mut got).unwrap();
            sock.write_all(&got).unwrap();
        });
        let parent = PathBuf::from(crate::DEFAULT_SCRATCH);
        std::fs::create_dir_all(&parent).unwrap();
        let relay = Relay::start(&parent, &[Endpoint { host: "127.0.0.1".into(), port }]).unwrap();
        let mut sock = UnixStream::connect(relay.dir().join(Relay::socket_name(0))).unwrap();
        sock.write_all(b"through the relay").unwrap();
        sock.shutdown(Shutdown::Write).unwrap();
        let mut back = String::new();
        sock.read_to_string(&mut back).unwrap();
        echo.join().unwrap();
        assert_eq!(back, "through the relay");
        assert_eq!(relay.report(), [format!("relay to 127.0.0.1:{port} carried 1 connection(s)")]);
        let dir = relay.dir().to_path_buf();
        drop(relay);
        assert!(!dir.exists(), "dropping the relay removes its directory");
    }

    #[test]
    fn an_unreachable_endpoint_is_reported() {
        let port = {
            let unused = TcpListener::bind("127.0.0.1:0").unwrap();
            unused.local_addr().unwrap().port()
        };
        let parent = PathBuf::from(crate::DEFAULT_SCRATCH);
        std::fs::create_dir_all(&parent).unwrap();
        let relay = Relay::start(&parent, &[Endpoint { host: "127.0.0.1".into(), port }]).unwrap();
        let mut sock = UnixStream::connect(relay.dir().join(Relay::socket_name(0))).unwrap();
        let mut back = Vec::new();
        let _ = sock.read_to_end(&mut back);
        // The error is recorded after the splice thread closes the socket.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while relay.report().len() < 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let report = relay.report();
        assert_eq!(report.len(), 2, "{report:?}");
        assert!(report[1].contains("connect to 127.0.0.1"), "{report:?}");
    }
}
