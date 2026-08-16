# The Claude Code peer lane — `claude-code-peer` + the CC actor

Kaijutsu talks to running Claude Code sessions over Claude Code's own
per-session unix socket. Amy's framing (2026-08-14), on learning the sockets
exist: *"I can start my claude code sessions in a mux and maybe we can message
them from within kaijutsu? that would be like a drift with `--drive`, really
cool and a neat compromise."*

It is a compromise in the good sense. Drift already means "inject into a
mailbox, flush at the next turn", so `--drive` is a **sink**, not a new
subsystem. And it spends nothing: pure local IPC, no vendor auth touched, no
CLI wrapped, no libpython, no metered tokens. Amy's sessions burn her
subscription seat because *she* started them under her own login; kaijutsu only
coordinates. That sidesteps the entire library-vs-binary policy question in
`python-player.md`.

**Provenance discipline.** Anthropic documents the *feature* (cross-session
messaging, CC 2.1.224+, macOS/Linux) but not the *wire*. Every protocol claim
below is tagged **[probed]** (observed live against CC 2.1.232 on moltar,
2026-08-14), **[source]** (read out of the shipped binary), or **[inferred]**.
Do not promote an inferred claim without probing it. Two claims were filed
wrong today and corrected only because someone re-probed them.

## Why this and not the alternatives

| direction | what it is | status |
|---|---|---|
| (A) harness drives kaijutsu | CC is host, kaijutsu the instrument | **works today** — `kaijutsu-mcp` + hooks, live since 2026-07-17 |
| (B) kaijutsu drives the harness | subscription-backed inference as a kernel LLM backend | undesigned; collides with kaish exec ownership; gated on Amy's policy read |
| **(C) kaijutsu messages a harness it does not drive** | **this doc** | protocol measured, send path built |

(C) is the cheapest and the only one with no policy question attached. See
`python-player.md` for (A)/(B).

## The protocol

### Discovery — the on-disk registry

`~/.claude/sessions/` holds two files per live session **[probed]**:

- **`<pid>.json`, mode 0644** — the public descriptor:
  `pid`, `sessionId`, `cwd`, `startedAt`, `procStart`, `version`,
  `peerProtocol`, `kind`, `entrypoint`, `messagingSocketPath`, `name`,
  `nameSince`, `updatedAt`, `status`, `statusUpdatedAt`.
- **`<pid>.<64-hex>.key`, mode 0600** — `{"peerToken": "<32 hex>", "procStart":
  "<digits>"}`. CC's source mints both a `peerToken` and a `childToken`
  (`randomBytes(16).toString("hex")`), and **which token you present selects
  your role**, `"peer"` vs `"child"` **[source]**.

`status` is an **opaque string** — `busy`, `idle`, `shell`, `waiting` all
observed **[probed]**. Do not model it as a closed enum; a fourth value turned
up the same afternoon the first three did.

There is **no wire discovery**. Nothing we have seen lets you *ask* a session
anything; `ListAgents` is filesystem-based **[inferred, strongly]**. The socket
is a **write-only inbox, not an RPC.**

### Liveness — the PID-reuse guard

`procStart` is field 22 (`starttime`) of `/proc/<pid>/stat`, clock ticks since
boot **[probed: cross-checked three ways against a live session]**. It exists
because a descriptor can outlive its process and the OS reuses PIDs. CC's own
code ranks candidates by this and reports `dead-owner` **[source]**.

Parsing trap: `comm` (field 2) is paren-wrapped and may itself contain spaces
and parens. Split after the **last** `)`, then index from there — `starttime`
is index 19 of that split. A naive whitespace split misindexes everything
after `comm`.

Report **four** states, not a boolean: alive / stale (pid reused) / gone /
**unknown**. `unknown` is load-bearing: folding an unreadable `/proc` or a
parse failure into "gone" would make every session read as dead if the format
ever shifted, and the roster would look plausibly empty rather than obviously
broken — a silent failure that reads as a true answer.

### Socket path

`$XDG_RUNTIME_DIR/cc-socks/<pid>.sock`, 0600, in a 0700 dir; one LISTEN per
interactive session **[probed]**. Falls back to `/tmp/cc-socks-<uid>/<pid>.sock`
only when the path would exceed the `sun_path` limit, plus a Windows named-pipe
branch **[source]**. Third-party write-ups all say `/tmp` — they are describing
the fallback.

### Frames

Newline-delimited JSON. A real sender emits, verbatim **[probed]**:

```json
{"msgV":1,"msg_id":"<uuid>","type":"user",
 "message":{"role":"user","content":"<envelope, see below>"},
 "priority":"next","from":"uds:/run/user/1000/cc-socks/<pid>.sock"}
```

- **`msgV` is a framing version**, distinct from the descriptor's
  `peerProtocol`. Two version fields; check both, fail closed on either.
- **`msg_id` is the uuid `SendMessage` returns to its caller** — a
  sender-generated correlation key. Use it; do not invent one.
- `priority: "next"` observed; rest of the enum unknown.
- `from` appears **both** top-level and inside the envelope.
- An `{"type":"auth","token":…}` frame may precede it. **Auth is not
  enforced** — see Security.
- **No ack.** Zero bytes come back on success **[probed]**. A read timeout is
  not an error.

### The attribution envelope, and its strict grammar

`content` must be the `<cross-session-message>` tag. The receiver's parser
regex **[source]**:

```
^<cross-session-message(?: from="…")?(?: from-session="…")?(?: hop-chain="…")?
 (?: from-name="[^"<>\n\r]+")?(?: from-mode="…")?>\n([\s\S]*)\n</cross-session-message>$
```

Three traps, all load-bearing:

1. Attributes are **order-sensitive**: `from`, `from-session`, `hop-chain`,
   `from-name`, `from-mode`.
2. The body **must** be newline-delimited *inside* the tag: `>\n` … `\n</`.
3. The parser **re-serializes what it extracted and rejects the parse unless it
   exactly equals the input** — a canonicalization check.

**A sloppy serializer degrades silently.** Our first probe omitted the
mandatory newlines: it delivered fine, was never parsed, and only *looked*
attributed because we had written the text ourselves. No error, message
delivered, attribution gone. This is the single most important hazard in the
protocol, and it is why the serializer's test round-trips through this regex
rather than asserting on a hand-written string.

Corollaries: reject a `from-name` containing `"`, `<`, `>`, or a newline rather
than escaping it (escaped text fails the canonicalization check — the same
silent downgrade in a disguise), and reject a body containing a literal
`</cross-session-message>` as envelope injection.

**Emit `from` if and only if it names a socket we are actually listening on.**
It is the address a replying agent uses. With no inbox, omitting it is correct
and the message still renders attributed by `from-name` **[probed]**; a
fabricated `uds:` path is worse than an absent one, because that string is
treated as a destination.

### Kaijutsu can be a listener — no registry squatting

**`SendMessage` delivers to an arbitrary `uds:` path with no entry in
`~/.claude/sessions/`** **[probed]** — bound a listener in a scratch dir,
addressed it by path, bytes arrived. So kaijutsu binds its own socket and
becomes a first-class **reply target** without writing a fake descriptor into
another program's private registry. Replies then work by CC's own documented
rule: *copy the incoming `from` attribute as your `to`.*

### Security posture

**Auth is not a gate** **[probed]**: the delivery to our listener carried no
auth frame, and a live session accepted a bare `{"type":"user",…}` with no
token. Therefore:

- **The socket's file permissions are the real boundary**, not the token.
  Anything on the box that can write the path can inject a turn into any
  session.
- Our own inbox lives in a **0700** dir and treats every inbound message as
  **unauthenticated input**.
- **Attribution is sender-asserted.** A well-formed tag naming a nonexistent
  `from` socket and a false `from-name` reaches the receiving model intact
  **[probed]**; the canonicalization check polices *format, not identity*.
  Nothing downstream may treat `fromName` as authorization.
- Keep *sending* auth anyway: a future CC that begins enforcing it would drop
  our messages **silently**, since there is no ack to notice with.
- Tokens are **read at send time and never stored** — not in the CRDT, not
  cached, not logged, not in a `Debug` impl. A per-session secret in a durable
  multi-writer log would outlive the session it authenticates and replicate to
  every client, for no gain: the 0600 keyfile is already the durable truth.
  `--dry-run` never opens the keyfile at all.

One genuine safety property: inbound peer messages **cannot** approve a pending
permission prompt, change config, or run slash commands in the receiving
session. So this cannot launder a permission decision past a session's own gate.

## Architecture

### The crate/kernel split

**`crates/claude-code-peer`** — protocol only. Frame types, canonical
serializer + parser, tag grammar, descriptor parsing, the `procStart` guard,
socket client and server. **No `kaijutsu-*` deps; dependencies injected.**
Unprefixed and publishable, exactly the `approval-ledger` precedent.

Two reasons beyond tidiness: it quarantines a foreign undocumented protocol
behind one seam, so a CC bump has one blast radius; and it is reusable across
the fleet (kaibo could use it). Nothing on crates.io covers the peer socket —
the existing crates (`claude-codes`, `claude-code-agent-sdk`,
`claude-code-acp-rs`) all target stream-json or ACP.

**The actor stays in the kernel** — session↔context mapping, drift integration,
presence rendering, principal stamping, gating. That is policy, not wire. It is
**IPC, not exec**, so kaish's `ExternalExec` does not own it; but the framing
and version checks get exactly one owner.

### Identity: turn an unauthenticated channel into a capability

CC defends its own sockets with a path inside a 0700 dir — the path *is* the
credential. Do the same deliberately: **give every registered peer its own
inbox path with an unguessable name.** Then which socket a message arrived on
identifies the sender.

That pairs with what `PeerRegistry` already guarantees: the principal is
**stamped server-side and never trusted from the client**. So inside the CRDT
log a CC session's messages carry a kernel-stamped principal rather than a
forgeable `from-name` — kaijutsu is *more* trustworthy than the transport it
rides.

### Reuse, not reinvention

- **Presence view**: copy `kaijutsu-kernel/src/midi_presence.rs` — ordered map
  + `AtomicU64` generation, read-only by construction, and its rule that *a
  missing entry reads as unknown, never absent*.
- **Peer registry exists**: `kaijutsu-kernel/src/peers.rs`; an MCP session
  already self-registers into it. A CC session becomes a `PeerConfig` there
  (nick from its stable `cc-*` label, `instance` from `CLAUDE_CODE_SESSION_ID`),
  **not** a fourth parallel map.
- **Durability line, already drawn by practice**: hooks and contexts are
  DB-first; presence/peers/shares are deliberately not durable, because a
  remembered liveness fact is a lie. Identity mapping → durable (already
  `contexts.label`); liveness/socket/token → in-memory.

### Traps in the surrounding code

1. **`freeze_mounts()`** (`kaijutsu-server/src/rpc.rs:1844`) — the mount must
   join the block above it, and frozen means **one backend per subtree**. Copy
   `ShareFs`'s internal router; never mount-per-session.
2. **`getattr` sizes the body and `read_all` reads exactly `attr.size`** — a
   roster that renders differently between the two silently truncates. Render
   deterministically, ordered, and bump `generation`.
3. **`PeerRegistry` resets on kernel restart**; `ActorHandle` already replays
   `peer_registration` on reconnect. Ride that replay or a session sits
   registered-and-invisible until its next hook fires.
4. **A new hook field moves three places at once** — `HookEvent`
   (`kaijutsu-mcp/src/hook_types.rs`), the native source map
   (`kaijutsu-mcp/src/hook_adapter.rs`), and
   `kaijutsu-mcp/tests/adapter_mapping.rs`, which asserts no field is silently
   dropped. The per-hook budget is **5s** and expiry degrades permissive, so
   registration must be one-shot (`Mutex::take`), never per-event.

## Test strategy

The hazard: **a fake we wrote can only confirm our own beliefs.** A green suite
against a fiction is the failure to design against, so two of these four layers
are anchored outside our own code.

1. **Golden captures** — verbatim real frames (we have them, including a full
   543-byte send) as fixtures. Assert our parser accepts them and our serializer
   is byte-identical to what a real sender emitted, modulo ids. The fixture came
   from reality, so it cannot drift into wishful thinking.
2. **A fake CC session** — binds a socket in a temp dir, writes a valid
   descriptor + keyfile, records what it receives, and can send replies to our
   inbox. Makes the actor end-to-end testable with no real CC. Trustworthy only
   because it is built to match (1).
3. **Loopback** — our sender into our own listener. Proves framing symmetry,
   proves nothing about CC compatibility. Necessary, insufficient, and easy to
   mistake for sufficient.
4. **Ignored live tests + a version canary** — `#[ignore]`d tests that drive the
   real path against a live session (two exist already). Record the CC version
   validated against; fail closed and loudly when `version`/`msgV`/
   `peerProtocol` moves past it. **Test that the canary fires** — a guard nobody
   has seen trip is a guess.

Layers 1 and 4 keep 2 and 3 honest. Do not ship the actor without both.

## What it gives

- **Sibling-agent coordination stops being ephemeral.** Cross-session
  conversation today lives in two transcripts and dies with them. Routed
  through the actor it is blocks in the CRDT log: durable, addressable,
  rendered in the app, searchable, forkable. "Crosstalk is a feature" gets
  infrastructure instead of goodwill.
- **Bidirectional CC integration with nothing wrapped**, and no metered spend.
- **Amy's melt plan lands**: CC sessions become drift targets, `kj drift <ctx>`
  works uniformly, `kj cc` retires.
- **The app becomes a switchboard** — who is alive, who is busy, message flow,
  delivery and receipt. Real events for the concepted trace-packet comets.
- **Fan-out**: "every session whose cwd is `~/src/kaijutsu`, re-read
  `signoff.md`."
- **It closes a live bug**: nothing in the tree reads `CLAUDE_CODE_*` today;
  agent identity is scraped from the newest transcript file, which at
  MCP-spawn time can name a *previous* session. `CLAUDE_CODE_SESSION_ID`
  removes the guess.
- **Eventually cross-machine without vendor relay.** CC routes cross-machine
  replies through Anthropic; kaijutsu already speaks SSH+capnp between
  machines, and the `hop-chain` field suggests receivers tolerate relayed
  messages. **[inferred — unprobed]**

## Status and build order

Built, on branch `cc-peer-roster` (worktree `~/src/wt/kj-cc-roster`), not
merged:

- `kj cc list` and `kj cc send [--dry-run]`, attribution validated against a
  real receiver.
- **`crates/claude-code-peer`** — the protocol-only crate this document's
  "Architecture" section calls for: descriptor scan, liveness guard, envelope
  codec, frame codec, send client, inbox listener. 58 tests + 2 ignored live
  probes, golden fixtures from a real session (see the crate's `tests/`).
- **`kj cc send` is ledger-gated** — the "Open decisions" question below is
  answered (Amy, 2026-08-16). `kj approve` answers the gate from any shell.

Order from here: **kernel wiring of the inbox** (the listener exists; connect
it as a drift/mailbox source, unlocks replies) → **truthful `from`** on the
sender once the kernel is listening → **presence** at `/run/cc`, built as a
*source* for the general live roster rather than a CC-specific store →
**per-peer paths + principal stamping** → **hooks** for consent and
session↔context mapping → **fan-out last**, now that the gate it was waiting
on exists.

## Open decisions

- ~~**Does `--drive` route through the approval ledger?**~~ **Resolved —
  yes** (Amy, 2026-08-16: *"yeah kj cc send should go through the ledger"*).
  Implemented as the first consumer of the approval-ledger gate
  (`crates/kaijutsu-kernel/src/kj/gate.rs`): durable ask row before any wait,
  fail-closed on `gate_wait_timeout`, answered via `kj approve`. The message
  body is a free variable in the gated statement, so allow-always rules can
  never be learned for it (ledger guarantee 3) — every send stays
  human-approved until that policy changes deliberately.
- `from-mode` enum beyond `prompting`; `priority` enum beyond `next`. Both
  unknown; hardcode the observed value and comment why.
- Whether a reply carries any reference to the original `msg_id`. Unprobed, and
  it determines how tightly the two-sided delivery record can join.
- How a mixed roster renders **which kind of knowledge each row is** —
  connection-bound presence vs TTL/last-seen. See `signoff.md` (live roster).
