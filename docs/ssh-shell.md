# SSH shell subsystem — `kaijutsu-shell`

**Status:** design note, not started. Pick this up once the SFTP read-path work
settles (it shares the subsystem plumbing, so let that land first to avoid churn).

**Goal:** a user `ssh`-es in and gets an interactive **kaish** session that starts
*contextless-ish* (a lobby), carries `kj`, and uses `kj attach` / `kj context switch`
to move into real contexts — working with each context's blocks through the VFS
(`/v/docs`, `/v/input`). The third SSH subsystem next to `kaijutsu-rpc` and `sftp`.

Since `docs/slash-v.md` made SFTP a **read-only** view, this shell is now the primary
interactive surface for two things SFTP no longer does: **privileged writes** (editing
`/etc/rc`, `/etc/config`) and **acting in** a context. It also mounts the read-only
introspection trees `/v/ctx` and `/v/session` from that note, so what SFTP shows
passively, the shell lets you explore *and* act in. The capability story is already
solved — see Capability below — so this subsystem is mostly plumbing.

## Why this is small

Two facts from the current architecture do most of the work:

1. **kaish is stateless, per-invocation.** All durable identity — *current context*,
   cwd, env — lives **outside** the kaish instance, in `SessionContextMap`
   (`runtime/context_engine.rs`, keyed by `session_id`) and `KernelDb`. The kaish
   instance is materialized, runs one command, and is dropped. See
   `mcp/servers/shell.rs:137` for the model-shell path that already does this per RPC
   call.

2. **The VFS reflows on the next materialization.** `/v/docs` and `/v/input` are
   mounted scoped to the context the `EmbeddedKaish` was materialized with
   (`runtime/embedded_kaish.rs:282`). So if a line runs `kj context switch`, the
   switch updates `SessionContextMap` (`runtime/kj_builtin.rs:550`,
   `KjResult::Switch`), and the *next* line's freshly-materialized kaish sees the new
   context — `/v/docs` now points at it. No new switching machinery needed.

So the interactive loop is:

```
loop {
    line = read_line(channel_stream)
    ctx  = SessionContextMap.current(session_id)        // may be the lobby
    kaish = materialize_context_kaish(session_id, ctx, principal, …)
    out  = kaish.execute_with_options(line, …)
    write(channel_stream, out)
    // kaish dropped here; session state persisted via KernelDb / SessionContextMap
}
```

The per-line statelessness that looks like a limitation is exactly what makes the
shell easy: a long-lived `session_id` + a read loop, not a long-lived kaish. (The
`ctx` line above is a sketch — see Capability for whether the context is *baked* here
at line start or *resolved live* per access so that a mid-line `kj attach` chains like
`cd`. Live is the recommendation.)

## The contextless gap

`materialize_context_kaish` (`kj/context_shell.rs:121`) **requires** a `context_id`
and bakes the `/v/docs` + `/v/input` mounts from it. There is no contextless
materialization today. Options, cheapest first:

- **Lobby = a well-known anchor context** (recommended — see Roots below). The user
  lands in a real, valid context; `/v/docs` is always live; only the *meaning* of the
  lobby is "you haven't attached to your work yet." Reuses everything.
- **True contextless materialization.** A new variant that skips the `/v/docs`/
  `/v/input` mounts and only enables the connect-y `kj` subcommands (the dispatcher
  at `kj/mod.rs:425` already supports `caller.context_id == None` for `kj context`,
  `kj attach`, `kj workspace`, `kj rc`…). More honest to "contextless," and the
  bare-VFS objection is now milder — the global `/v/ctx`/`/v/session` mounts keep the
  lobby browsable even with no context-scoped docs — but it's still a new code path
  with no caps and no `/v/docs` until the user attaches.

## Roots / anchor contexts — the open decision

**Do not reuse `kj context scratch` as the lobby.** Scratch is a **global singleton**:
it's `get-or-create` on the unique label `"scratch"` (`kj/context.rs:755`), persisted
like any context (`insert_context_with_document`), and survives restart. There is
exactly *one* scratch context for the whole kernel, so every SSH login would share it.

There is no genesis/root context constant. The only well-known system context today is
**`lost+found`**, lazily minted by `ensure_lost_found()` with `PrincipalId::system()`
(`drift.rs:548`) and re-adopted at cold-start (`rpc.rs:1289`,
`drift.adopt_lost_found`). That `ensure_*` / `adopt_*` pair is the pattern to copy for
a lobby anchor.

Lineage for reference: `forked_from` column (root = `None`) traversed by
`fork_lineage()`, plus a structural DAG in the `context_edges` table
(`structural_parents()` / `structural_children()`). A lobby anchor would naturally be
a root (`forked_from = None`).

Decision to make before coding:

- **Single shared lobby** — one system-owned anchor (`ensure_lobby()`, mirrors
  `lost+found`). Simplest. Everyone shares one lobby context; fine if the lobby is
  just a launchpad and real work always happens in attached contexts.
- **Per-principal home** — a `get-or-create` keyed on the authenticated principal
  (label like `home:<principal>` or a dedicated table/column, since the `"scratch"`
  label is a global unique index and won't scale per-user as-is). More like a Unix
  home dir; each user gets a private landing context. Preferred if the lobby should
  accumulate any per-user state.

Recommendation: start with **per-principal home** via a small `ensure_home(principal)`
following the `ensure_lost_found` pattern — it's the behavior a login shell implies and
avoids the shared-state surprise. Fall back to a single shared lobby only if that's
more code than we want for v1.

## VFS shape — settled by `/v/ctx`, and the two-cursor model

The first draft listed "multi-context browse tree (new work, defer past v1)" as an
open decision. That work **already exists** as `/v/ctx` (`docs/slash-v.md`) — a
read-only, all-contexts tree with an `index`. So the shell doesn't build it; it just
**mounts the global trees** (`/v/ctx`, `/v/session`) alongside the context-scoped
`/v/docs`/`/v/input`. The materialization at `kj/context_shell.rs:121` bakes the
scoped mounts from `context_id`; the global trees are context-independent and mount
once regardless of current context (so even the lobby can browse everything).

That leaves the shell with **two independent cursors**, and keeping them distinct is
the whole ergonomic:

- **Acting context** — set by `kj context switch` / `kj attach`, stored in
  `SessionContextMap`. Drives your capabilities *and* what the **writable** `/v/docs`
  reflows to. This is who you're *acting as*.
- **cwd** — set by `cd`, persisted per session (durable L1, like context vars). This
  is where you're *looking*.

They don't move together. `cd /v/ctx/<shard>/<ctx>` is **switch-stable** — you browse
any context's blocks read-only without acting as it. `/v/docs` is **switch-relative** —
it re-points when you switch. Browsing (`/v/ctx`, read-only) and acting (`/v/docs` +
caps, current context) are deliberately separate surfaces. Ship attach-and-reflow for
the acting surface; `/v/ctx` is the browse surface, already specced.

## Capability — writes are gated by your current context

Per `docs/slash-v.md`, there is no per-session capability token: a privileged write is
authorized by the context it **runs in** (`ExecContext.context_id`), and
`context_allows_rc_write(ctx)` (`file_tools/guard.rs:71`) is the one gate. The shell
gets this for free because each line's kaish is materialized with the current context.

**Where the current context comes from — the decision that matters.** A "line" is one
kaish *materialization*: read a line, build one kaish, run the whole line, drop it.
The question is whether the context is **baked** at materialization or **resolved
live** from `SessionContextMap` per access/write. This surfaces as an asymmetry:

- `cd` **chains** mid-line (`cd /foo ; ls` lists `/foo`) — cwd is kaish-internal state
  the `;` sequence threads.
- `kj attach` / `kj context switch` does **not** chain if the context is baked —
  because the context lives in the mount table (`/v/docs` reflow) and the `ExecContext`
  (caps), both fixed before the line runs. So `cd /foo ; kj attach X ; mv /foo/bar /baz`
  would run `mv` in the *pre-attach* context.

That asymmetry is a footgun: a user who just saw `cd` chain expects `attach` to. It
also contradicts slash-v's **per-operation join** — the write should join whatever
context is current *at that operation* (statement-level), which is exactly what the
`mv` wants. So the **target is live resolution**: resolve the context-scoped mounts
*and* the guard's context from `SessionContextMap` at access/write time, and `attach`
chains mid-line just like `cd`.

The invariant either way: **bake both or resolve both — never mix.** Live caps with a
baked `/v/docs` (or vice-versa) is the worst case — your caps say `X` but the docs
still show the old context. So v1 picks one: live-resolution (statement-level, matches
Unix; the materialize path already carries `session_id`, so it's "read current context
at access time" rather than "capture once") — *or*, only if live proves materially
more code, baked-per-line as a documented constraint (switch on its own line, before
you act). Recommend live.

(Editing rc requires switching to a context whose `context_type` carries
`RcWrite`/`ConfigWrite` — `rc`/`admin`/`mcp`; the per-principal home is not
privileged.) Because the acting context must be legible before you act, **rendering
the current context label in the prompt is a v1 requirement**, not just "output
framing" — see Usage patterns.

## Usage patterns (feedback through real sessions)

Playing the shell out, smallest surprises first:

- **Orient → find work.** Land in `home`. `cat /v/ctx/index` (TSV) and `grep` for a
  label → get the `path` column → `kj attach <id>`. `/v/ctx` is mounted regardless of
  current context, so this works from the lobby.
- **Switch, then write.** Privileged edits are two lines (switch, then write) per the
  Capability rule. Interactive `vi`/`edit` needs a PTY (deferred), so v1 rc editing is
  the non-interactive `kj rc edit <path> --content <body>`.
- **Watch a hot block.** `cat /v/ctx/<shard>/<ctx>/blocks/<key>/content` snapshots at
  open and each line re-materializes, so "watch it grow" is a poll loop — no follow
  mode in v1. Acceptable; the `generation` stamp is what a poller keys on.
- **Two cursors in play.** `cd /v/ctx/…` to browse while still acting as `home`; your
  prompt shows `home`, your `pwd` shows where you're looking. Switching contexts moves
  `/v/docs` but not your `pwd`.
- **Many hands.** Two logins both `kj context switch feature-x` and both write — CRDT
  multi-writer, both visible in `ls -l /v/session`. This is the literal shared
  instrument. **Principal is the Unix model:** the authorship lane
  (`BlockId.principal_id`) is the authenticated *user's* principal — two logins by one
  user are two ttys for one uid, same lane, no per-session principal. The connection
  (`instance` / `session_id`) distinguishes `/v/session` rows and rides traces for
  observability, but never enters authorship.

## Wiring (where the code goes)

- **Dispatch:** add `const SSH_SHELL_SUBSYSTEM: &str = "kaijutsu-shell";` and a third
  match arm in `ConnectionHandler::subsystem_request`
  (`kaijutsu-server/src/ssh.rs:763-852`), next to the `kaijutsu-rpc` and `sftp` arms.
  Every exit path must call `channel_success` / `channel_failure` (client uses
  `want_reply`).
- **Handler:** a new `crate::shell::ShellSession { principal, … }` that takes
  `chan.into_stream()` and runs the read loop. Model it on the **SFTP** handler shape
  (`kaijutsu-server/src/sftp.rs`), which runs on the ambient runtime — *if* the kaish
  execute path is `Send` (see below). If not `Send`, copy the RPC pattern instead:
  dedicated OS thread + current-thread runtime + `LocalSet` (`ssh.rs:418`,
  `spawn_rpc_thread`).
- **Session id:** mint one `session_id` for the connection so `SessionContextMap`
  carries the current context across lines.
- **Lobby:** on session open, set `SessionContextMap[session_id] = ensure_home(principal)`
  (or the shared lobby), then run the loop.
- **Registry:** register the connection in `PeerRegistry` (`peers.rs:103`) with the new
  `kind = "shell"` field so it appears in `/v/session` (`docs/slash-v.md`); resolve
  `/v/session/self` to this session's key; deregister on disconnect.
- **Global mounts:** materialization must add the context-independent `/v/ctx` +
  `/v/session` mounts (not just the scoped `/v/docs`/`/v/input`) — they're the browse +
  roster surfaces and must be live even in the lobby.

## Pre-flight checks before coding

- **Is the kaish execute path `Send`?** Decides SFTP-style (ambient runtime) vs
  RPC-style (dedicated thread). The capnp RPC path is `!Send` and *needs* the thread;
  kaish may be fine on the ambient runtime — verify, don't assume.
- **Line editing / PTY.** v1 can be line-at-a-time over the raw channel stream (read a
  line, run it, write output) with no PTY. A real PTY (history, cursor, raw mode) means
  implementing `pty_request` on `ConnectionHandler` (none exists today) and threading
  PTY metadata into the handler — defer.
- **Output framing.** Decide how `execute_with_options` output (stdout/stderr/exit
  code, possibly structured) maps onto the byte stream and prompt rendering.

## Reference: key files

| What | Where |
|------|-------|
| Subsystem dispatch | `kaijutsu-server/src/ssh.rs:763-852` |
| RPC subsystem (thread + LocalSet) | `kaijutsu-server/src/ssh.rs:418` (`spawn_rpc_thread`) |
| SFTP handler (ambient runtime, VFS access) | `kaijutsu-server/src/sftp.rs` |
| kaish materialization (requires context_id) | `kj/context_shell.rs:121` |
| Model-shell over RPC (per-call materialize) | `mcp/servers/shell.rs:137` |
| Current-context map | `runtime/context_engine.rs:17` (`SessionContextMap`) |
| Context switch persistence | `runtime/kj_builtin.rs:550` (`KjResult::Switch`) |
| VFS mounts scoped to context | `runtime/embedded_kaish.rs:282` (`/v/docs`, `/v/input`) |
| `kj` no-context dispatch gate | `kj/mod.rs:425` |
| Scratch context (global singleton) | `kj/context.rs:755` (`context_scratch`) |
| `lost+found` anchor pattern to copy | `drift.rs:548` (`ensure_lost_found`), `rpc.rs:1289` (adopt) |
