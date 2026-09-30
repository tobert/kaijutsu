//! A small ACP v1 client over a child process's stdio.
//!
//! Framing is newline-delimited JSON-RPC 2.0, one message per line, which is
//! what the `agent-client-protocol` crate's stdio transport reads and writes.
//! The client is hand-rolled rather than taken from that crate so a caller can
//! assert on raw notification shapes.
//!
//! Reader threads drain the child's stdout and stderr, so neither pipe can fill
//! and stall the agent. Every wait has a deadline, and every failure carries the
//! tail of the agent's stderr.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::{Value, json};

/// How many trailing stderr lines an error message carries.
const STDERR_TAIL_LINES: usize = 40;

/// How often [`AcpClient::pump_until_state`] rechecks its condition while no
/// message arrives.
pub const POLL: Duration = Duration::from_millis(50);

/// The command that starts an ACP agent: a program, its arguments, extra
/// environment, and the directory it is launched in.
#[derive(Debug, Clone)]
pub struct AgentCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: Option<PathBuf>,
}

impl AgentCommand {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self { program: program.into(), args: Vec::new(), env: Vec::new(), cwd: None }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }
}

/// How the client answers one `session/request_permission`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionAnswer {
    /// Select the option of kind `allow_once`.
    Allow,
    /// Select the option of kind `allow_always`.
    AllowAlways,
    /// Select the option of kind `reject_once`.
    Deny,
    /// Select the option of kind `reject_always`.
    DenyAlways,
    /// Answer with the `cancelled` outcome.
    Cancel,
    /// Send no response yet; answer later with [`AcpClient::release_held`].
    Hold,
}

impl PermissionAnswer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::AllowAlways => "allow_always",
            Self::Deny => "deny",
            Self::DenyAlways => "deny_always",
            Self::Cancel => "cancel",
            Self::Hold => "hold",
        }
    }
}

/// Which answers the client gives to permission requests.
#[derive(Debug, Clone)]
pub enum PermissionPolicy {
    /// Answer each request with the next queued answer. A request that
    /// arrives after the queue is empty is answered `cancelled` and recorded
    /// as unexpected.
    Queue(VecDeque<PermissionAnswer>),
    /// Answer every request the same way.
    Always(PermissionAnswer),
}

impl Default for PermissionPolicy {
    fn default() -> Self {
        Self::Queue(VecDeque::new())
    }
}

/// One `session/request_permission` the agent sent, and what we did with it.
#[derive(Debug, Clone)]
pub struct PermissionRecord {
    pub params: Value,
    /// The answer the policy chose, or `None` when the queue was empty.
    pub answer: Option<PermissionAnswer>,
    /// The option id we selected, when the answer selected one.
    pub option_id: Option<String>,
    /// Why the answer could not be applied as the policy asked, if it could not.
    pub problem: Option<String>,
}

/// One tool call as the update stream last described it: the `tool_call`
/// that announced it, with every later `tool_call_update` folded in.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallSeen {
    pub id: String,
    pub title: String,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub raw_input: Option<Value>,
    /// The text of every `content` entry reported for the call, joined.
    pub output: String,
}

/// A running ACP agent and the client state around it.
pub struct AcpClient {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    stderr: Arc<Mutex<String>>,
    /// Bytes the agent has written to stdout, counted as the reader thread
    /// reads them, whether or not a caller has handled them yet.
    stdout_bytes: Arc<AtomicUsize>,
    next_id: i64,
    timeout: Duration,
    policy: PermissionPolicy,
    updates: Vec<Value>,
    permissions: Vec<PermissionRecord>,
    /// Responses that arrived while we waited for a different id.
    parked: Vec<Value>,
    /// Permission requests answered [`PermissionAnswer::Hold`], oldest
    /// first: the JSON-RPC id and the index of their record.
    held: VecDeque<(Value, usize)>,
    /// Print every message in both directions to stderr.
    trace: bool,
}

impl AcpClient {
    /// Spawn the agent. `timeout` bounds every later wait for a response.
    pub fn spawn(command: &AgentCommand, timeout: Duration) -> Result<Self> {
        let mut cmd = Command::new(&command.program);
        cmd.args(&command.args);
        for (key, value) in &command.env {
            cmd.env(key, value);
        }
        if let Some(cwd) = &command.cwd {
            cmd.current_dir(cwd);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn {}", command.program.display()))?;

        let stdout = child.stdout.take().context("the child has no piped stdout")?;
        let (tx, lines) = channel();
        let stdout_bytes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&stdout_bytes);
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                counter.fetch_add(line.len() + 1, Ordering::SeqCst);
                if tx.send(line).is_err() {
                    return;
                }
            }
        });

        let stderr_pipe = child.stderr.take().context("the child has no piped stderr")?;
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr_pipe).lines().map_while(Result::ok) {
                let mut held = sink.lock().expect("stderr sink poisoned");
                held.push_str(&line);
                held.push('\n');
            }
        });

        Ok(Self {
            stdin: child.stdin.take(),
            child,
            lines,
            stderr,
            stdout_bytes,
            next_id: 1,
            timeout,
            policy: PermissionPolicy::default(),
            updates: Vec::new(),
            permissions: Vec::new(),
            parked: Vec::new(),
            held: VecDeque::new(),
            trace: false,
        })
    }

    /// Print every message in both directions to this process's stderr.
    pub fn set_trace(&mut self, trace: bool) {
        self.trace = trace;
    }

    pub fn set_permission_policy(&mut self, policy: PermissionPolicy) {
        self.policy = policy;
    }

    /// Everything the agent has written to stderr so far.
    pub fn stderr(&self) -> String {
        self.stderr.lock().expect("stderr sink poisoned").clone()
    }

    /// The last lines of stderr, for failure messages.
    pub fn stderr_tail(&self) -> String {
        let all = self.stderr();
        let lines: Vec<&str> = all.lines().collect();
        let start = lines.len().saturating_sub(STDERR_TAIL_LINES);
        lines[start..].join("\n")
    }

    /// The agent's process id: the `podman` client's in contained mode.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Bytes the agent has written to stdout so far. stdout is the ACP wire,
    /// so an agent that was never asked anything leaves this at zero.
    pub fn stdout_bytes(&self) -> usize {
        self.stdout_bytes.load(Ordering::SeqCst)
    }

    /// Wait up to `within` for `needle` to appear on the agent's stderr.
    pub fn wait_for_stderr(&self, needle: &str, within: Duration) -> Result<()> {
        self.wait_for_stderr_since(0, needle, within)
    }

    /// Wait up to `within` for `needle` to appear on the agent's stderr after
    /// its first `mark` bytes; take `mark` from `stderr().len()` before the
    /// action whose effect it reports.
    pub fn wait_for_stderr_since(&self, mark: usize, needle: &str, within: Duration) -> Result<()> {
        let deadline = Instant::now() + within;
        loop {
            if self.stderr().get(mark..).is_some_and(|tail| tail.contains(needle)) {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!("{needle:?} never appeared on the agent's stderr within {within:?}\n--- agent stderr ---\n{}", self.stderr());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// `session/update` params, in arrival order.
    pub fn updates(&self) -> &[Value] {
        &self.updates
    }

    /// Permission requests, in arrival order.
    pub fn permissions(&self) -> &[PermissionRecord] {
        &self.permissions
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        if self.trace {
            eprintln!("acp-fleet -> {message}");
        }
        let stdin = self.stdin.as_mut().context("stdin is already closed")?;
        writeln!(stdin, "{message}").context("write to the agent's stdin")?;
        stdin.flush().context("flush the agent's stdin")
    }

    /// Send a request and return its id without waiting for the response.
    pub fn send_request(&mut self, method: &str, params: Value) -> Result<i64> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        Ok(id)
    }

    /// Wait for the response to request `id`, answering anything the agent
    /// asks in the meantime. A JSON-RPC error response is an `Err`.
    pub fn wait_response(&mut self, id: i64, waiting_for: &str) -> Result<Value> {
        if let Some(pos) = self.parked.iter().position(|m| response_id(m) == Some(id)) {
            let message = self.parked.remove(pos);
            return self.result_of(message, waiting_for);
        }
        let deadline = Instant::now() + self.timeout;
        loop {
            let message = self.next_message(deadline, waiting_for)?;
            match response_id(&message) {
                Some(got) if got == id => return self.result_of(message, waiting_for),
                Some(_) => self.parked.push(message),
                None => self.dispatch(message)?,
            }
        }
    }

    fn result_of(&self, message: Value, waiting_for: &str) -> Result<Value> {
        if let Some(error) = message.get("error") {
            bail!("{waiting_for} failed: {error}\n--- agent stderr (tail) ---\n{}", self.stderr_tail());
        }
        Ok(message.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Read and handle messages until none has arrived for `quiet`. Fails
    /// when the agent is still talking after `within`.
    pub fn pump_until_quiet(&mut self, quiet: Duration, within: Duration, what: &str) -> Result<()> {
        let deadline = Instant::now() + within;
        let mut last = Instant::now();
        loop {
            let now = Instant::now();
            if now.duration_since(last) >= quiet {
                return Ok(());
            }
            if now >= deadline {
                bail!(
                    "the agent was still sending messages {within:?} after {what}\n--- agent stderr (tail) ---\n{}",
                    self.stderr_tail()
                );
            }
            let wait = (last + quiet).min(deadline).saturating_duration_since(now);
            match self.lines.recv_timeout(wait) {
                Ok(line) => {
                    last = Instant::now();
                    if self.trace {
                        eprintln!("acp-fleet <- {line}");
                    }
                    let message: Value = serde_json::from_str(&line)
                        .with_context(|| format!("the agent wrote a stdout line that is not JSON: {line:?}"))?;
                    if response_id(&message).is_some() {
                        self.parked.push(message);
                    } else {
                        self.dispatch(message)?;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => bail!(
                    "the agent closed stdout after {what}\n--- agent stderr (tail) ---\n{}",
                    self.stderr_tail()
                ),
            }
        }
    }

    /// Read and handle messages until `done` holds for the updates seen so
    /// far, which it is also checked against before reading. Fails after
    /// `within`, or when the response to `request` arrives first: the
    /// request ended without what the caller waited for.
    pub fn pump_until(
        &mut self,
        request: Option<i64>,
        what: &str,
        within: Duration,
        done: impl Fn(&[Value]) -> bool,
    ) -> Result<()> {
        self.pump_until_state(request, what, within, |client| done(&client.updates))
    }

    /// [`Self::pump_until`] for a condition on the whole client, such as
    /// held permission requests, or on something outside the wire, such as a
    /// file: `done` is also checked every [`POLL`] while no message arrives.
    pub fn pump_until_state(
        &mut self,
        request: Option<i64>,
        what: &str,
        within: Duration,
        mut done: impl FnMut(&Self) -> bool,
    ) -> Result<()> {
        let deadline = Instant::now() + within;
        loop {
            if done(self) {
                return Ok(());
            }
            if let Some(response) = self.parked.iter().find(|m| request.is_some() && response_id(m) == request) {
                bail!(
                    "the request ended before {what}: {response}\n--- agent stderr (tail) ---\n{}",
                    self.stderr_tail()
                );
            }
            let now = Instant::now();
            if now >= deadline {
                bail!("timed out after {within:?} waiting for {what}\n--- agent stderr (tail) ---\n{}", self.stderr_tail());
            }
            let line = match self.lines.recv_timeout(POLL.min(deadline - now)) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => bail!(
                    "the agent closed stdout while we waited for {what}\n--- agent stderr (tail) ---\n{}",
                    self.stderr_tail()
                ),
            };
            if self.trace {
                eprintln!("acp-fleet <- {line}");
            }
            let message: Value = serde_json::from_str(&line)
                .with_context(|| format!("the agent wrote a stdout line that is not JSON: {line:?}"))?;
            if response_id(&message).is_some() {
                self.parked.push(message);
            } else {
                self.dispatch(message)?;
            }
        }
    }

    /// Send a request and wait for its result.
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.send_request(method, params)?;
        self.wait_response(id, method)
    }

    fn next_message(&mut self, deadline: Instant, waiting_for: &str) -> Result<Value> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match self.lines.recv_timeout(remaining) {
            Ok(line) => {
                if self.trace {
                    eprintln!("acp-fleet <- {line}");
                }
                serde_json::from_str(&line)
                    .with_context(|| format!("the agent wrote a stdout line that is not JSON: {line:?}"))
            }
            Err(RecvTimeoutError::Timeout) => Err(anyhow!(
                "timed out after {:?} waiting for {waiting_for}\n--- agent stderr (tail) ---\n{}",
                self.timeout,
                self.stderr_tail()
            )),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!(
                "the agent closed stdout while we waited for {waiting_for}\n--- agent stderr (tail) ---\n{}",
                self.stderr_tail()
            )),
        }
    }

    /// Record a notification, or answer a request the agent sent. Every
    /// request gets a reply, since an unanswered one would stall the turn.
    fn dispatch(&mut self, message: Value) -> Result<()> {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Ok(());
        };
        let method = method.to_string();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = message.get("id").cloned() else {
            if method == "session/update" {
                self.updates.push(params);
            }
            return Ok(());
        };
        if method == "session/request_permission" {
            let (outcome, record) = self.answer_permission(params);
            self.permissions.push(record);
            let Some(outcome) = outcome else {
                self.held.push_back((id, self.permissions.len() - 1));
                return Ok(());
            };
            return self.send(&json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": outcome}}));
        }
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("acp-fleet does not implement {method}")},
        }))
    }

    /// The outcome to send for a request, or `None` to hold it.
    fn answer_permission(&mut self, params: Value) -> (Option<Value>, PermissionRecord) {
        let answer = match &mut self.policy {
            PermissionPolicy::Queue(queue) => queue.pop_front(),
            PermissionPolicy::Always(answer) => Some(*answer),
        };
        let mut record = PermissionRecord { params, answer, option_id: None, problem: None };
        let Some(answer) = answer else {
            record.problem = Some("no permission answer was queued for this request".into());
            return (Some(json!({"outcome": "cancelled"})), record);
        };
        if answer == PermissionAnswer::Hold {
            return (None, record);
        }
        let outcome = outcome_for(answer, &mut record);
        (Some(outcome), record)
    }

    /// How many permission requests are held unanswered, counting only those
    /// whose record index in [`Self::permissions`] is at least `since`.
    pub fn held_count(&self, since: usize) -> usize {
        self.held.iter().filter(|(_, index)| *index >= since).count()
    }

    /// Answer the oldest held permission request whose record index is at
    /// least `since` with `answer`, which must not be
    /// [`PermissionAnswer::Hold`]. The answer and any problem applying it
    /// replace the ones on the request's record.
    pub fn release_held(&mut self, since: usize, answer: PermissionAnswer) -> Result<()> {
        if answer == PermissionAnswer::Hold {
            bail!("release_held needs an answer, not another hold");
        }
        let position = self
            .held
            .iter()
            .position(|(_, index)| *index >= since)
            .context("no permission request is held")?;
        let (id, index) = self.held.remove(position).expect("the position was just found");
        let record = &mut self.permissions[index];
        record.answer = Some(answer);
        let outcome = outcome_for(answer, record);
        self.send(&json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": outcome}}))
    }

    pub fn initialize(&mut self) -> Result<Value> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {},
                "clientInfo": {"name": "acp-fleet", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
    }

    /// Open a session rooted at `cwd` and return its id.
    pub fn new_session(&mut self, cwd: &Path) -> Result<String> {
        let result = self.request("session/new", json!({"cwd": cwd, "mcpServers": []}))?;
        result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("session/new returned no sessionId: {result}"))
    }

    /// Send a prompt and return its id; pair with [`Self::wait_response`].
    pub fn start_prompt(&mut self, session: &str, text: &str) -> Result<i64> {
        self.send_request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
        )
    }

    /// Send a prompt and wait for the turn to end.
    pub fn prompt(&mut self, session: &str, text: &str) -> Result<Value> {
        let id = self.start_prompt(session, text)?;
        self.wait_response(id, "session/prompt")
    }

    /// Ask the agent to stop the session's current turn.
    pub fn cancel(&mut self, session: &str) -> Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session}}))
    }

    /// Close stdin, the way a client that is done disconnects, and wait up to
    /// `within` for the agent to exit. Kill it if it does not.
    pub fn shutdown(&mut self, within: Duration) -> Result<ExitStatus> {
        drop(self.stdin.take());
        self.wait_exit("stdin closing", within)
    }

    /// Wait up to `within` for the agent to exit on its own, after `after`
    /// (a signal, a failure it was told to stage). Kill it if it does not.
    pub fn wait_exit(&mut self, after: &str, within: Duration) -> Result<ExitStatus> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.child.try_wait().context("poll the agent")? {
                return Ok(status);
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                bail!(
                    "the agent did not exit within {within:?} of {after}; killed it\n\
                     --- agent stderr (tail) ---\n{}",
                    self.stderr_tail()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The outcome that gives `answer` to the request `record` holds, noting on
/// `record` the option chosen or why none could be. `Hold` has no outcome and
/// is answered `cancelled`.
fn outcome_for(answer: PermissionAnswer, record: &mut PermissionRecord) -> Value {
    let cancelled = json!({"outcome": "cancelled"});
    let kind = match answer {
        PermissionAnswer::Cancel | PermissionAnswer::Hold => return cancelled,
        PermissionAnswer::Allow => "allow_once",
        PermissionAnswer::AllowAlways => "allow_always",
        PermissionAnswer::Deny => "reject_once",
        PermissionAnswer::DenyAlways => "reject_always",
    };
    match select_option(&record.params, kind) {
        Some(option) => {
            record.option_id = Some(option.clone());
            record.problem = None;
            json!({"outcome": "selected", "optionId": option})
        }
        None => {
            record.problem = Some(format!(
                "the request offered no option of kind {kind:?}: {}",
                record.params.get("options").unwrap_or(&Value::Null)
            ));
            cancelled
        }
    }
}

/// The id of a response message; `None` for a request or a notification.
fn response_id(message: &Value) -> Option<i64> {
    if message.get("method").is_some() {
        return None;
    }
    message.get("id").and_then(Value::as_i64)
}

/// The first offered option of kind `want`.
fn select_option(params: &Value, want: &str) -> Option<String> {
    params.get("options")?.as_array()?.iter().find_map(|option| {
        let kind = option.get("kind")?.as_str()?;
        if kind == want {
            option.get("optionId")?.as_str().map(str::to_string)
        } else {
            None
        }
    })
}

/// The text of a tool call update's `content` entries, joined.
fn content_text(update: &Value) -> String {
    let Some(entries) = update.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    entries.iter().filter_map(|e| e.pointer("/content/text").and_then(Value::as_str)).collect()
}

/// Every `agent_message_chunk` text in `updates`, joined.
pub fn agent_text(updates: &[Value]) -> String {
    updates
        .iter()
        .filter(|u| u.pointer("/update/sessionUpdate").and_then(Value::as_str) == Some("agent_message_chunk"))
        .filter_map(|u| u.pointer("/update/content/text").and_then(Value::as_str))
        .collect()
}

/// The tool calls in `updates`, in announcement order, each with its later
/// `tool_call_update`s folded in.
pub fn tool_calls(updates: &[Value]) -> Vec<ToolCallSeen> {
    let mut calls: Vec<ToolCallSeen> = Vec::new();
    for params in updates {
        let Some(update) = params.get("update") else { continue };
        let field = |name: &str| update.get(name).and_then(Value::as_str).map(str::to_string);
        let Some(id) = field("toolCallId") else { continue };
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("tool_call") => calls.push(ToolCallSeen {
                id,
                title: field("title").unwrap_or_default(),
                kind: field("kind"),
                status: field("status"),
                raw_input: update.get("rawInput").cloned(),
                output: content_text(update),
            }),
            Some("tool_call_update") => {
                if let Some(call) = calls.iter_mut().find(|c| c.id == id) {
                    if let Some(title) = field("title") {
                        call.title = title;
                    }
                    if let Some(kind) = field("kind") {
                        call.kind = Some(kind);
                    }
                    if let Some(status) = field("status") {
                        call.status = Some(status);
                    }
                    if let Some(input) = update.get("rawInput") {
                        call.raw_input = Some(input.clone());
                    }
                    call.output.push_str(&content_text(update));
                }
            }
            _ => {}
        }
    }
    calls
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(body: Value) -> Value {
        json!({"sessionId": "s", "update": body})
    }

    #[test]
    fn agent_text_joins_only_agent_chunks() {
        let updates = vec![
            update(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "he"}})),
            update(json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "XX"}})),
            update(json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "YY"}})),
            update(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "llo"}})),
        ];
        assert_eq!(agent_text(&updates), "hello");
    }

    #[test]
    fn tool_call_updates_fold_into_the_call_they_name() {
        let updates = vec![
            update(json!({"sessionUpdate": "tool_call", "toolCallId": "a", "title": "write", "kind": "edit", "status": "pending"})),
            update(json!({"sessionUpdate": "tool_call", "toolCallId": "b", "title": "shell", "status": "pending"})),
            update(json!({"sessionUpdate": "tool_call_update", "toolCallId": "a", "status": "completed"})),
            update(json!({"sessionUpdate": "tool_call_update", "toolCallId": "b", "status": "failed",
                "content": [{"type": "content", "content": {"type": "text", "text": "refused: use shell_write"}}]})),
        ];
        let calls = tool_calls(&updates);
        let summary: Vec<(&str, Option<&str>)> =
            calls.iter().map(|c| (c.title.as_str(), c.status.as_deref())).collect();
        assert_eq!(summary, vec![("write", Some("completed")), ("shell", Some("failed"))]);
        assert_eq!(calls[0].kind.as_deref(), Some("edit"));
        assert_eq!(calls[1].output, "refused: use shell_write");
    }

    #[test]
    fn options_are_chosen_by_kind_not_position() {
        let params = json!({"options": [
            {"optionId": "no", "name": "Deny", "kind": "reject_once"},
            {"optionId": "always", "name": "Always allow", "kind": "allow_always"},
            {"optionId": "yes", "name": "Allow", "kind": "allow_once"},
        ]});
        assert_eq!(select_option(&params, "allow_once").as_deref(), Some("yes"));
        assert_eq!(select_option(&params, "allow_always").as_deref(), Some("always"));
        assert_eq!(select_option(&params, "reject_once").as_deref(), Some("no"));
        assert_eq!(select_option(&params, "reject_always"), None);
        assert_eq!(select_option(&json!({"options": []}), "allow_once"), None);
    }

    #[test]
    fn a_released_answer_selects_its_option_and_clears_nothing_else() {
        let mut record = PermissionRecord {
            params: json!({"options": [
                {"optionId": "yes", "kind": "allow_once"},
                {"optionId": "no", "kind": "reject_once"},
            ]}),
            answer: Some(PermissionAnswer::Hold),
            option_id: None,
            problem: None,
        };
        assert_eq!(outcome_for(PermissionAnswer::Deny, &mut record), json!({"outcome": "selected", "optionId": "no"}));
        assert_eq!(record.option_id.as_deref(), Some("no"));
        assert_eq!(outcome_for(PermissionAnswer::Hold, &mut record), json!({"outcome": "cancelled"}));
    }

    #[test]
    fn responses_are_told_apart_from_requests() {
        assert_eq!(response_id(&json!({"jsonrpc": "2.0", "id": 3, "result": {}})), Some(3));
        assert_eq!(response_id(&json!({"jsonrpc": "2.0", "id": 3, "method": "x"})), None);
        assert_eq!(response_id(&json!({"jsonrpc": "2.0", "method": "session/update"})), None);
    }
}
