//! Terminal-fit integration harness support: an ephemeral kaijutsu-server
//! plus a real `kaijutsu-tui` binary running inside a pty, parsed with a
//! second terminal emulator (`vt100`).
//!
//! `EphemeralServer` boots the server on its own thread with its own
//! current-thread runtime and `LocalSet` (Cap'n Proto RPC is `!Send`, so it
//! cannot share the test's own runtime the way an in-process kernel test
//! does — this one talks real SSH over a real socket to a real subprocess).
//! `TuiSession` spawns the binary into a pty and answers the one terminal
//! query ratatui's inline viewport cannot proceed without: the cursor
//! position request (`ESC [ 6 n`).

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

/// Probes run one at a time. Each owns its own kernel, tempdir, port and
/// pty, so there is no shared state to race on — but seven real SSH
/// handshakes plus seven child processes in flight at once made parallel
/// runs flaky, so every probe takes this guard first. A probe that panicked
/// while holding it must not poison the rest of the run.
pub fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ────────────────────────────────────────────────────────────────────────────
// The ephemeral server
// ────────────────────────────────────────────────────────────────────────────

/// A `kaijutsu-server` bound to an ephemeral loopback port, running on its
/// own OS thread for the life of this handle. Dropping it cancels the
/// server and joins the thread — every probe gets its own kernel and its
/// own tempdir (`SshServerConfig::ephemeral`'s `TempDirGuard` cleans that up
/// in turn), so probes never share state.
pub struct EphemeralServer {
    pub addr: SocketAddr,
    cancel: Arc<tokio::sync::Notify>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl EphemeralServer {
    /// Start the server and block until it has actually bound a port.
    pub fn start() -> Self {
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let cancel = Arc::new(tokio::sync::Notify::new());
        let cancel_task = cancel.clone();

        let join = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build ephemeral-server runtime");
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind ephemeral port");
                let bound = listener.local_addr().expect("read bound addr");
                addr_tx.send(bound).expect("report bound addr");

                let config = kaijutsu_server::SshServerConfig::ephemeral(bound.port());
                let server = kaijutsu_server::SshServer::new(config);
                tokio::select! {
                    res = server.run_on_listener(listener) => {
                        if let Err(e) = res {
                            eprintln!("ephemeral kaijutsu-server exited: {e}");
                        }
                    }
                    _ = cancel_task.notified() => {}
                }
            });
        });

        let addr = addr_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("ephemeral server bound a port");
        Self { addr, cancel, join: Some(join) }
    }
}

impl Drop for EphemeralServer {
    fn drop(&mut self) {
        self.cancel.notify_waiters();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Generate an ephemeral Ed25519 key (the same generation
/// `kaijutsu_client::KeySource::ephemeral()` does) and write it to
/// `<dir>/id_ed25519` in OpenSSH format, for `kaijutsu-tui --key <path>`.
/// Returns the key file path.
pub fn write_ephemeral_key(dir: &Path) -> PathBuf {
    use russh::keys::{Algorithm, PrivateKey, ssh_key::LineEnding};

    let key = PrivateKey::random(&mut rand_v10::rng(), Algorithm::Ed25519)
        .expect("generate ephemeral ed25519 key");
    let encoded = key.to_openssh(LineEnding::LF).expect("encode key as OpenSSH");
    let path = dir.join("id_ed25519");
    std::fs::write(&path, encoded.as_bytes()).expect("write key file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    path
}

// ────────────────────────────────────────────────────────────────────────────
// The pty session
// ────────────────────────────────────────────────────────────────────────────

/// The real `kaijutsu-tui` binary, running inside a pty, with its output
/// parsed live by a `vt100::Parser`.
pub struct TuiSession {
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    parser: Arc<Mutex<vt100::Parser>>,
    reader: Option<std::thread::JoinHandle<()>>,
    rows: u16,
    cols: u16,
}

/// How much scrollback the parser keeps. Generous relative to what any
/// probe here prints — a few completed blocks plus the picker's own churn.
const SCROLLBACK_LINES: usize = 500;

impl TuiSession {
    /// Spawn `kaijutsu-tui` against `server`, authenticating with the key at
    /// `key_path`, inside a `rows`x`cols` pty.
    pub fn spawn(server: SocketAddr, key_path: &Path, rows: u16, cols: u16) -> Self {
        Self::spawn_after_newlines(server, key_path, rows, cols, 0)
    }

    /// Like [`spawn`](Self::spawn), but the cursor starts `newlines` rows
    /// down the screen when the binary takes over — the shape of launching
    /// from a shell whose prompt sits mid-screen. A fresh pty starts at
    /// row 0, so a `sh` wrapper prints the newlines and then `exec`s the
    /// binary in the same pty.
    pub fn spawn_after_newlines(
        server: SocketAddr,
        key_path: &Path,
        rows: u16,
        cols: u16,
        newlines: u16,
    ) -> Self {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .expect("open pty");

        let mut cmd = if newlines == 0 {
            CommandBuilder::new(env!("CARGO_BIN_EXE_kaijutsu-tui"))
        } else {
            let mut cmd = CommandBuilder::new("sh");
            cmd.arg("-c");
            cmd.arg(format!(
                "i=0; while [ \"$i\" -lt {newlines} ]; do echo; i=$((i + 1)); done; exec \"$0\" \"$@\""
            ));
            cmd.arg(env!("CARGO_BIN_EXE_kaijutsu-tui"));
            cmd
        };
        cmd.arg("--host");
        cmd.arg(server.ip().to_string());
        cmd.arg("--port");
        cmd.arg(server.port().to_string());
        cmd.arg("--user");
        cmd.arg("test_user");
        cmd.arg("--insecure");
        cmd.arg("--key");
        cmd.arg(key_path);
        cmd.arg("--context");
        cmd.arg("probe");
        cmd.arg("--connect-timeout");
        cmd.arg("30");
        // stdout is the viewport, stderr carries diagnostics — and in a pty
        // both land on the same stream, so keep the log level quiet enough
        // that a warn-or-above line is the only thing that could interleave
        // with a frame (`crates/kaijutsu-tui/src/main.rs` module doc).
        cmd.env("RUST_LOG", "warn");
        cmd.env("TERM", "xterm-256color");

        let child = pair.slave.spawn_command(cmd).expect("spawn kaijutsu-tui");
        // Drop the parent's slave-side handle so the master sees EOF when
        // the child exits, rather than holding the pty open forever.
        drop(pair.slave);

        let reader = pair.master.try_clone_reader().expect("clone pty reader");
        let writer: Box<dyn Write + Send> = pair.master.take_writer().expect("take pty writer");
        let writer = Arc::new(Mutex::new(writer));

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK_LINES)));

        let reader_parser = parser.clone();
        let reader_writer = writer.clone();
        let reader = std::thread::spawn(move || read_loop(reader, reader_parser, reader_writer));

        Self {
            child,
            master: pair.master,
            writer,
            parser,
            reader: Some(reader),
            rows,
            cols,
        }
    }

    /// Write raw bytes to the pty as if typed: `"hello"`, `"\x01\""` for
    /// Ctrl+A then `"`, `"\x03\x03"` for Ctrl+C twice, `"\x1b"` for Esc.
    pub fn send(&self, raw: &str) {
        let mut w = self.writer.lock().expect("writer lock");
        w.write_all(raw.as_bytes()).expect("write to pty");
        w.flush().expect("flush pty writer");
    }

    /// Resize the pty (which raises SIGWINCH for the child, same as a real
    /// terminal resize) and the parser's idea of the screen size to match.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .expect("resize pty");
        self.parser.lock().expect("parser lock").screen_mut().set_size(rows, cols);
        self.rows = rows;
        self.cols = cols;
    }

    /// The visible screen, one string per row, left-to-right, no formatting.
    pub fn screen_text(&self) -> Vec<String> {
        let p = self.parser.lock().expect("parser lock");
        p.screen().rows(0, self.cols).collect()
    }

    /// Every scrollback line the parser has buffered, oldest first. Walks
    /// the parser's scrollback offset from its maximum down to 1, reading
    /// the row that becomes visible at row 0 on each step — `vt100`
    /// exposes no direct "give me the whole buffer" call.
    pub fn scrollback_text(&self) -> Vec<String> {
        self.history_snapshot().0
    }

    /// `(scrollback, visible)` captured under a single lock acquisition, so
    /// the two agree on exactly the same moment. Calling `screen_text()` and
    /// `scrollback_text()` back to back does not: the reader thread can feed
    /// the parser more bytes — and more content can scroll off the visible
    /// area into scrollback — in the gap between two separate lock
    /// acquisitions, which showed up as lines vanishing from both halves at
    /// once in an early version of the picker probe. Take this whenever a
    /// caller needs both halves to describe one instant.
    pub fn history_snapshot(&self) -> (Vec<String>, Vec<String>) {
        let mut p = self.parser.lock().expect("parser lock");
        let cols = self.cols;
        let visible: Vec<String> = p.screen().rows(0, cols).collect();

        p.screen_mut().set_scrollback(usize::MAX);
        let total = p.screen().scrollback();
        let mut scrollback = Vec::with_capacity(total);
        let mut offset = total;
        while offset > 0 {
            p.screen_mut().set_scrollback(offset);
            if let Some(row) = p.screen().rows(0, cols).next() {
                scrollback.push(row);
            }
            offset -= 1;
        }
        p.screen_mut().set_scrollback(0);
        (scrollback, visible)
    }

    /// Poll `pred` against the live screen until it returns true or
    /// `timeout` elapses. Returns whether it matched.
    pub fn wait_until<F>(&self, timeout: Duration, pred: F) -> bool
    where
        F: Fn(&vt100::Screen) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let p = self.parser.lock().expect("parser lock");
                if pred(p.screen()) {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Block until the child exits or `timeout` elapses.
    pub fn wait_for_exit(&mut self, timeout: Duration) -> Option<portable_pty::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A labeled, line-numbered rendering of the current screen, for
    /// assertion failure messages — a failing probe should print the screen
    /// it actually saw.
    pub fn dump(&self, label: &str) -> String {
        let mut out = format!("--- {label} ({}x{}) ---\n", self.rows, self.cols);
        for (i, line) in self.screen_text().iter().enumerate() {
            out.push_str(&format!("{i:3} | {line}\n"));
        }
        out
    }
}

impl Drop for TuiSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// Feed pty output into the parser, answering every cursor-position query
/// (`ESC [ 6 n`, CSI DSR) as it arrives — the gotcha this harness exists to
/// solve. `ratatui`'s inline viewport issues this query on creation and on
/// every resize (`Terminal::with_options`, and `set_viewport_height` in
/// `crates/kaijutsu-tui/src/run.rs`) and blocks until it is answered; a bare
/// `vt100::Parser` never answers one, so an unanswered query hangs the
/// child until crossterm's own timeout fails the run with "the cursor
/// position could not be read within a normal duration".
///
/// Bytes are fed to the parser incrementally, up to and including each
/// query, so the reported position reflects the screen state at the moment
/// the query was made rather than whatever arrived after it in the same
/// read. A small tail is held back between reads in case a query's bytes
/// are split across two `read()` calls.
fn read_loop(
    mut reader: Box<dyn Read + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
) {
    const DSR_CURSOR: &[u8] = b"\x1b[6n";
    let mut buf = [0u8; 4096];
    let mut carry: Vec<u8> = Vec::new();

    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        carry.extend_from_slice(&buf[..n]);

        loop {
            let Some(pos) = find_subslice(&carry, DSR_CURSOR) else { break };
            let split_at = pos + DSR_CURSOR.len();
            let head: Vec<u8> = carry.drain(..split_at).collect();
            let (row, col) = {
                let mut p = parser.lock().expect("parser lock");
                p.process(&head);
                p.screen().cursor_position()
            };
            let reply = format!("\x1b[{};{}R", row + 1, col + 1);
            if let Ok(mut w) = writer.lock() {
                let _ = w.write_all(reply.as_bytes());
                let _ = w.flush();
            }
        }

        // No complete query left in `carry`. Feed everything except a short
        // tail that might be the front half of a query split across reads.
        let keep = carry.len().saturating_sub(DSR_CURSOR.len() - 1);
        if keep > 0 {
            let head: Vec<u8> = carry.drain(..keep).collect();
            parser.lock().expect("parser lock").process(&head);
        }
    }

    if !carry.is_empty() {
        parser.lock().expect("parser lock").process(&carry);
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
