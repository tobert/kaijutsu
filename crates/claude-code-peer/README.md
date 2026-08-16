# claude-code-peer

Rust client and listener for Claude Code's per-session peer messaging
socket: the local unix socket a running Claude Code (2.1.224+) session opens
to accept attributed messages from another process on the same machine.

**This is a reverse-engineered protocol.** Anthropic documents the feature
(cross-session messaging) but has not published the wire format. Nothing
here is official, affiliated with, or endorsed by Anthropic, and the wire
can change under this crate without notice. Treat every claim in this crate
by its provenance tag — see "Provenance" below — before relying on it.

## What it does

- **Discovers** live sessions by reading `~/.claude/sessions/`, one
  `<pid>.json` descriptor per session, and guards against PID reuse by
  comparing `/proc/<pid>/stat` start time against the value recorded in the
  descriptor (`liveness` module).
- **Sends** an attributed message into a session's inbox: builds the
  `<cross-session-message>` envelope, reads that session's one-time key
  file, and writes two newline-delimited JSON frames (`auth`, then `user`)
  to its unix socket (`client` module).
- **Listens**: binds a unix socket of your own and accepts the same framing,
  so your process can be a first-class reply target without registering a
  fake session in Claude Code's own registry (`server` module).

It does not run Claude Code, does not manage its process, and does not read
anything back over the socket — the protocol has no acknowledgement (see
"Platform assumptions" below).

## Platform assumptions

- **Linux only, today.** Liveness checks `/proc/<pid>/stat` directly; there
  is no macOS or Windows liveness backend even though Claude Code itself
  ships for macOS too.
- **Local unix domain sockets only** — nothing here opens a network socket.
  A message can only reach a session that a process on the same machine, as
  the same user or one that can read its socket, can write to.
- **No authentication boundary.** Claude Code accepts an `auth` frame, but
  enforcement was not observed live; the real boundary is the unix socket's
  own file permissions. This crate's [`server`] inbox binds its directory
  `0700` and its socket `0600` and treats every inbound message as
  unauthenticated input — attribution (`from-name`) is sender-asserted, and
  nothing in this crate treats it as authorization. An embedder that needs
  authorization has to add its own layer on top.

## Provenance

Every protocol claim in this crate's doc comments is tagged one of three
ways, and the tag is the whole point — it tells you how much to trust the
claim without re-deriving it yourself:

- **`[probed]`** — observed live against a real Claude Code session
  (measured against 2.1.233; see [`PROBED_AGAINST_CC`]).
- **`[source]`** — read directly out of the shipped Claude Code binary.
- **`[inferred]`** — reasoned from the above, not directly observed. Do not
  promote an inferred claim to probed without actually probing it — two
  claims in this crate were filed as probed before someone re-checked them
  and found they were wrong.

Three parts of the protocol are genuinely unprobed, not just undocumented:

- Whether a reply frame carries any reference back to the original
  `msg_id` it is replying to. Unknown either way.
- The `from-mode` attribute's full range. Only `"prompting"` has been
  observed; the rest of the enum is unknown, so this crate hardcodes that
  one value rather than modeling an enum it cannot verify.
- The `priority` field's full range. Only `"next"` has been observed, same
  treatment.

A field this crate has not personally probed against a real session is
never asserted as fact — see the module docs (`frame`, `descriptor`,
`envelope`) for where each specific claim is tagged.

## Usage

### Sending into a live session

```rust,no_run
use claude_code_peer::{default_sessions_dir, resolve_target, scan_sessions_dir, send_message};

let sessions_dir = default_sessions_dir().expect("HOME is set");
let scan = scan_sessions_dir(&sessions_dir).expect("read the session registry");
let session = resolve_target(&scan.sessions, "my-session-name").expect("find the target session");

let outcome = send_message(
    &sessions_dir,
    session,
    "my-tool-name",   // from_name: attributed to the receiving session
    "hello there",    // message body
    None,             // reply_socket: Some(addr) only if you're listening (see below)
).expect("send");
println!("sent {} bytes, msg_id {}", outcome.bytes_written, outcome.msg_id.as_str());
```

`send_message` fails closed before writing anything: `resolve_target`
refuses an ambiguous name, and the send itself refuses a session whose
liveness check did not come back `Alive` or whose `peerProtocol` this crate
does not speak — a stale PID may belong to an unrelated process by the time
you write to it.

### Listening for replies

```rust,no_run
use claude_code_peer::Inbox;

async fn listen() {
    let mut inbox = Inbox::bind(std::path::Path::new("/run/user/1000/my-app"), "inbox.sock")
        .expect("bind the inbox socket");
    let reply_address = inbox.uds_address(); // pass this as `reply_socket` above
    println!("listening as {reply_address}");

    while let Some(msg) = inbox.recv().await {
        println!("from {:?}: {}", msg.from_socket, msg.content);
    }
}
```

Everything an `Inbox` hands you is unauthenticated input from whatever wrote
to the socket path — see "Platform assumptions" above before treating
`from_socket` or the envelope's `from-name` as identity.

## Development

This crate is developed inside [kaijutsu](https://github.com/tobert/kaijutsu),
a larger monorepo (see `repository` above), but has no dependency on any
`kaijutsu-*` crate and is built to stand alone. The fuller protocol writeup —
the same `[probed]`/`[source]`/`[inferred]` claims as the doc comments here,
gathered in one place — lives at `docs/cc-peer.md` in that repository. If
you're extending this crate, keep tagging new claims the same way, and
re-probe before promoting an inferred one to probed — that discipline is
most of what makes this crate trustworthy to depend on.
