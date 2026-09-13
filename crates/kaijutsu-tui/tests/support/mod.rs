//! Terminal-fit integration harness support: an ephemeral kaijutsu-server
//! plus a real `kaijutsu-tui` binary running inside a pty, parsed with a
//! second terminal emulator (`vt100`).
//!
//! `EphemeralServer` boots the server on its own thread with its own
//! current-thread runtime and `LocalSet` (Cap'n Proto RPC is `!Send`, so it
//! cannot share the test's own runtime the way an in-process kernel test
//! does — this one talks real SSH over a real socket to a real subprocess).
//! `TuiSession` spawns the binary into a pty, answers any cursor-position
//! request (`ESC [ 6 n`) and counts them: the client owns the alternate
//! screen for the session and must never send one
//! (`the_client_never_asks_where_the_cursor_is`).

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
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
    /// Cursor-position requests seen since the session started. The client
    /// sends none; the count is the probe's receipt.
    cursor_queries: Arc<AtomicUsize>,
    /// Synchronized updates begun (`ESC [ ? 2026 h`) less those ended
    /// (`ESC [ ? 2026 l`). Zero whenever the client is between frames — a
    /// terminal left inside an update shows nothing at all.
    sync_depth: Arc<Mutex<i64>>,
    /// Alternate scroll (DECSET 1007) enables and disables seen so far, in
    /// that order — the client turns it on with the screen and off again on
    /// the way out.
    alt_scroll: Arc<Mutex<(usize, usize)>>,
    /// The decoded text of every clipboard write (`ESC ] 52 ; c ; <base64>
    /// BEL`) seen so far. A yank is the only caller, so this is exactly what
    /// went to the clipboard.
    clipboard: Arc<Mutex<Vec<String>>>,
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
        Self::spawn_with(server, key_path, rows, cols, 0, &[])
    }

    /// Like [`spawn`](Self::spawn), with extra environment for the binary —
    /// the probe hooks `run.rs` reads at startup (`KAIJUTSU_TUI_PROBE_PANIC`).
    pub fn spawn_with_env(server: SocketAddr, key_path: &Path, rows: u16, cols: u16, env: &[(&str, &str)]) -> Self {
        Self::spawn_with(server, key_path, rows, cols, 0, env)
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
        Self::spawn_with(server, key_path, rows, cols, newlines, &[])
    }

    fn spawn_with(
        server: SocketAddr,
        key_path: &Path,
        rows: u16,
        cols: u16,
        newlines: u16,
        env: &[(&str, &str)],
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
        // The log goes beside the key rather than into the state directory
        // of whoever runs the suite; a failing probe can read it there.
        cmd.arg("--log");
        cmd.arg(key_path.with_file_name("tui.log"));
        cmd.env("RUST_LOG", "warn");
        cmd.env("TERM", "xterm-256color");
        for (key, value) in env {
            cmd.env(key, value);
        }

        let child = pair.slave.spawn_command(cmd).expect("spawn kaijutsu-tui");
        // Drop the parent's slave-side handle so the master sees EOF when
        // the child exits, rather than holding the pty open forever.
        drop(pair.slave);

        let reader = pair.master.try_clone_reader().expect("clone pty reader");
        let writer: Box<dyn Write + Send> = pair.master.take_writer().expect("take pty writer");
        let writer = Arc::new(Mutex::new(writer));

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK_LINES)));

        let cursor_queries = Arc::new(AtomicUsize::new(0));
        let sync_depth = Arc::new(Mutex::new(0i64));
        let alt_scroll = Arc::new(Mutex::new((0usize, 0usize)));
        let clipboard = Arc::new(Mutex::new(Vec::new()));
        let reader_parser = parser.clone();
        let reader_writer = writer.clone();
        let reader_count = cursor_queries.clone();
        let counts = Counts {
            sync_depth: sync_depth.clone(),
            alt_scroll: alt_scroll.clone(),
            clipboard: clipboard.clone(),
        };
        let reader = std::thread::spawn(move || {
            read_loop(reader, reader_parser, reader_writer, reader_count, counts)
        });

        Self {
            child,
            master: pair.master,
            writer,
            parser,
            reader: Some(reader),
            cursor_queries,
            sync_depth,
            alt_scroll,
            clipboard,
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

    /// `(scrollback, visible)` captured under a single lock acquisition, so
    /// the two agree on exactly the same moment.
    ///
    /// The client owns the alternate screen, whose scrollback a terminal
    /// does not keep, so the scrollback half describes the screen the shell
    /// had: what a probe reads after the client has exited, or what it
    /// printed before taking the screen.
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

    /// Cursor-position requests (`ESC [ 6 n`) the client has sent so far.
    pub fn cursor_queries(&self) -> usize {
        self.cursor_queries.load(Ordering::SeqCst)
    }

    /// Synchronized updates the client began and has not ended. Any value
    /// but zero at rest means a frame left the terminal holding its paint.
    pub fn sync_updates_open(&self) -> i64 {
        *self.sync_depth.lock().expect("sync depth lock")
    }

    /// `(enables, disables)` of alternate scroll (DECSET 1007) seen so far.
    pub fn alternate_scroll(&self) -> (usize, usize) {
        *self.alt_scroll.lock().expect("alt scroll lock")
    }

    /// Every clipboard write (OSC 52) the client has made so far, decoded.
    pub fn clipboard(&self) -> Vec<String> {
        self.clipboard.lock().expect("clipboard lock").clone()
    }

    /// Rows of the transcript area painted in reverse — the reader's own
    /// line while the view is off the tail.
    pub fn reader_rows(&self, transcript_rows: usize) -> Vec<usize> {
        let p = self.parser.lock().expect("parser lock");
        reader_rows(p.screen(), transcript_rows)
    }

    /// Rows of the transcript area carrying a background — the `v` mark's
    /// paint, which nothing else above the band uses.
    pub fn marked_rows(&self, transcript_rows: usize) -> Vec<usize> {
        let p = self.parser.lock().expect("parser lock");
        marked_rows(p.screen(), transcript_rows)
    }

    /// Whether the client has turned bracketed paste on (DECSET 2004).
    pub fn bracketed_paste(&self) -> bool {
        self.parser.lock().expect("parser lock").screen().bracketed_paste()
    }

    /// Whether the parsed terminal is on the alternate screen buffer.
    pub fn on_alternate_screen(&self) -> bool {
        self.parser.lock().expect("parser lock").screen().alternate_screen()
    }

    /// Whether the pty's line discipline is cooked — `ICANON` and `ECHO`
    /// both on, the way a shell expects to find it. Read through the master
    /// side, which on Linux reports the slave's termios, so it can be read
    /// after the child has exited.
    #[cfg(unix)]
    pub fn cooked(&self) -> Option<bool> {
        use std::os::unix::io::RawFd;
        let fd: RawFd = self.master.as_raw_fd()?;
        // SAFETY: `termios` is plain data and `tcgetattr` only writes into it.
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut term) } != 0 {
            return None;
        }
        Some(term.c_lflag & libc::ICANON != 0 && term.c_lflag & libc::ECHO != 0)
    }

    /// The child process's pid, for a probe that needs to inspect its OS
    /// process state directly (`Ctrl+Z`'s suspend: `portable_pty` has no
    /// "is this job stopped" query of its own).
    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
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

/// Feed pty output into the parser, counting and answering every
/// cursor-position query (`ESC [ 6 n`, CSI DSR) as it arrives.
///
/// The client sends none: a full-screen viewport never asks where the
/// cursor is, which is what `the_client_never_asks_where_the_cursor_is`
/// proves. The answer stays so that a query from anywhere is a counted
/// fact rather than a hang — crossterm fails a blocked read after two
/// seconds with "the cursor position could not be read within a normal
/// duration", which would read as a timeout rather than as the query it is.
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
    queries: Arc<AtomicUsize>,
    counts: Counts,
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
        counts.fold(&buf[..n]);

        loop {
            let Some(pos) = find_subslice(&carry, DSR_CURSOR) else { break };
            let split_at = pos + DSR_CURSOR.len();
            let head: Vec<u8> = carry.drain(..split_at).collect();
            let (row, col) = {
                let mut p = parser.lock().expect("parser lock");
                p.process(&head);
                p.screen().cursor_position()
            };
            queries.fetch_add(1, Ordering::SeqCst);
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

/// The tallies the read loop keeps over the raw byte stream, for the
/// sequences `vt100` does not model.
///
/// Counted on the raw stream rather than through the parser. A read boundary
/// inside a sequence would miscount; the client writes each one in a single
/// `execute!` or a single `write_all`, so the bytes arrive whole.
struct Counts {
    sync_depth: Arc<Mutex<i64>>,
    alt_scroll: Arc<Mutex<(usize, usize)>>,
    clipboard: Arc<Mutex<Vec<String>>>,
}

impl Counts {
    fn fold(&self, bytes: &[u8]) {
        let count = |needle: &[u8]| occurrences(bytes, needle);

        let delta = count(b"\x1b[?2026h") as i64 - count(b"\x1b[?2026l") as i64;
        if delta != 0 {
            let mut depth = self.sync_depth.lock().expect("sync depth lock");
            // Ending an update the terminal never began is a no-op for the
            // terminal, so it is one here: the depth floors at zero.
            *depth = (*depth + delta).max(0);
        }

        let (on, off) = (count(b"\x1b[?1007h"), count(b"\x1b[?1007l"));
        if on + off > 0 {
            let mut seen = self.alt_scroll.lock().expect("alt scroll lock");
            seen.0 += on;
            seen.1 += off;
        }

        self.fold_clipboard(bytes);
    }

    /// Decode every complete OSC 52 write in `bytes`: the base64 between
    /// `ESC ] 52 ; c ;` and the BEL the client terminates it with.
    fn fold_clipboard(&self, bytes: &[u8]) {
        const OSC52: &[u8] = b"\x1b]52;c;";
        let mut rest = bytes;
        while let Some(at) = find_subslice(rest, OSC52) {
            let body = &rest[at + OSC52.len()..];
            let Some(end) = body.iter().position(|b| *b == 0x07) else { break };
            if let Some(text) = base64_decode(&body[..end]) {
                self.clipboard.lock().expect("clipboard lock").push(text);
            }
            rest = &body[end..];
        }
    }
}

/// Rows of the transcript area painted in reverse — the reader's own line.
///
/// Free rather than a method, because a `wait_until` predicate is handed the
/// screen with the parser already locked and that lock is not reentrant.
pub fn reader_rows(screen: &vt100::Screen, transcript_rows: usize) -> Vec<usize> {
    painted_rows(screen, transcript_rows, |cell| cell.inverse())
}

/// Rows of the transcript area carrying a background — the `v` mark's paint.
pub fn marked_rows(screen: &vt100::Screen, transcript_rows: usize) -> Vec<usize> {
    painted_rows(screen, transcript_rows, |cell| {
        !cell.inverse() && cell.bgcolor() != vt100::Color::Default
    })
}

fn painted_rows<F: Fn(&vt100::Cell) -> bool>(
    screen: &vt100::Screen,
    transcript_rows: usize,
    paint: F,
) -> Vec<usize> {
    let (rows, cols) = screen.size();
    (0..transcript_rows.min(usize::from(rows)))
        .filter(|&row| {
            (0..cols).any(|col| {
                screen
                    .cell(u16::try_from(row).unwrap_or(u16::MAX), col)
                    .is_some_and(&paint)
            })
        })
        .collect()
}

/// Standard padded base64, the alphabet `copy::osc52_sequence` writes.
/// `None` on anything that is not one — a probe should say "that was not
/// base64" rather than compare against a silently emptied string.
fn base64_decode(data: &[u8]) -> Option<String> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    if data.is_empty() || !data.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    for chunk in data.chunks(4) {
        let pad = chunk.iter().filter(|b| **b == b'=').count();
        let mut n = 0u32;
        for (i, byte) in chunk.iter().enumerate() {
            let six = if *byte == b'=' { 0 } else { ALPHABET.iter().position(|a| a == byte)? as u32 };
            n |= six << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    String::from_utf8(out).ok()
}

fn occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    haystack.windows(needle.len()).filter(|w| *w == needle).count()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
