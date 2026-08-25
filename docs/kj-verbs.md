# `kj` verbs and their destructiveness

Every addressable `kj <verb> <sub>` with an honest read on whether it changes
durable state and whether `--confirm` gates it. The **what it does** column is
the reflected one-line description clap emits from `kj_command()`
(`crates/kaijutsu-kernel/src/kj/mod.rs:859`) — read off the live kernel's
`--help`, not written by hand here. Long descriptions are trimmed at the first
clause; nothing is paraphrased.

`--confirm` is the gate. It is a bare root-level flag
(`kj/mod.rs:875-880`), stripped from argv before dispatch and carried to each
handler as `KjCaller.confirmed` (`runtime/kj_builtin.rs:644-645`, `:686`). A
verb is confirm-gated only where a handler tests that field. There are exactly
**six** such sites in the whole `kj` surface — an exhaustive
`grep -rn "\.confirmed\b"` over `crates/kaijutsu-kernel/src/kj/` returns
nothing else outside `#[cfg(test)]` fixtures. Because the flag is extracted in
one place shared by the kaish builtin, the MCP tool, and the RPC facade, the
gate applies uniformly across all three surfaces.

**A `no` in the confirm-gated column does not mean safe.** Most destructive
verbs here are ungated: `kj cast remove` cascades its slots away, `kj backend
remove` deletes a row outright, `kj rc rm` can destroy a user-authored script
with no recovery, `kj drift edge rm` hard-deletes an edge with no capability
check at all. Capability checks (`require_cap`) exist on many write paths, but
they are a different mechanism — an ergonomic loadout nudge, not a
confirmation — and are not reported in the confirm-gated column.

## What was and was not verified live

Every row's `mutates?` and `confirm-gated?` value is derived from reading the
handler source, not from running the verb. A live kernel holding real work runs
on this machine, so **no verb was executed except `--help`**, which is pure
clap reflection with no side effects. That means no row was confirmed by
observing an actual mutation. Specifically **not** run: everything named
remove, delete, archive, reset, rm, clear, discard, prune, retag, swap, stop,
kill, panic, reseed, or forget; every `--confirm`-gated verb, with or without
the flag; and all of `fork`, `drive`, `play`, `cp`, `drift push/pull/merge/flush`,
`transport *`, `editor *`, `block edit/append/create/status`, and `db backup`.
`--confirm` was never passed to anything.

## Verbs

| verb | what it does | mutates? | confirm-gated? |
|---|---|---|---|
| `kj context list` | List active contexts (or the fork DAG with --tree) | no | no |
| `kj context info` | Show a context's metadata (default: current) | no | no |
| `kj context prompt` | Render the system prompt this context actually gets | no | no |
| `kj context current` | Print the current context | no | no |
| `kj context switch` | Switch the session to another context | yes | no |
| `kj context create` | Create a new context. Label is positional or `--name` | yes | no |
| `kj context scratch` | Get-or-create the well-known "scratch" context | yes | no |
| `kj context rebind` | Re-run the `create` rc lifecycle on a context left with no usable loadout | yes | no |
| `kj context set` | Apply settable config to an existing context (default: current) | yes | no |
| `kj context unset` | Remove an env var from a context, or clear its assigned cast | yes | no |
| `kj context log` | Show fork lineage from a context up to root (default: current) | no | no |
| `kj context move` | Reparent a context under a new parent | yes | no |
| `kj context rename` | Rename a context — set its label (default: current) | yes | no |
| `kj context archive` | Soft-delete a context (latched) | yes-destructive | **yes** — context.rs:1719 |
| `kj context conclude` | Conclude a context — mark this work "done". Reversible via fork; not latched, not destructive | yes | no |
| `kj context promote` | Promote a context into ring 0 ("active"). Not latched, not capability-gated | yes | no |
| `kj context demote` | Push a context outward one step on the demote ladder | yes-destructive | no |
| `kj context pause` | Set the "suspend activity" flag | yes | no |
| `kj context resume` | Clear the "suspend activity" flag | yes | no |
| `kj context remove` | Permanently delete a context (latched) | yes-destructive | **yes** — context.rs:1964 |
| `kj context retag` | Move a label to a different context (latched) | yes | **yes** — context.rs:2075 |
| `kj context hydrate` | Set or clear the conversation hydration window | yes | no |
| `kj workspace list` | List all workspaces | no | no |
| `kj workspace show` | Show a workspace's description and paths | no | no |
| `kj workspace create` | Create a workspace with optional description and initial paths | yes | no |
| `kj workspace add` | Add a path to an existing workspace | yes | no |
| `kj workspace bind` | Bind a workspace to a context | yes | no |
| `kj workspace remove` | Archive a workspace (latched) | yes-destructive | **yes** — workspace.rs:318 |
| `kj preset list` | List all presets | no | no |
| `kj preset show` | Show details for a preset | no | no |
| `kj preset save` | Create or update a preset | yes | no |
| `kj preset remove` | Remove a preset (latched) | yes-destructive | **yes** — preset.rs:299 |
| `kj preset reseed` | Restore the reserved factory presets (full/window/spawn) to their embedded defaults | yes | no |
| `kj backend list` | List configured backends | no | no |
| `kj backend show` | Show one backend, its models, and its resolved key source | no | no |
| `kj backend set` | Create or update a backend (upsert on `name`) | yes | no |
| `kj backend remove` | Remove a backend. Refused while a cast slot or alias still points at it | yes-destructive | no |
| `kj backend model set` | Pin (or re-pin) one model's metadata on a backend | yes | no |
| `kj backend model remove` | Drop one model's metadata row | yes | no |
| `kj backend default` | Show or set the kernel-wide LLM defaults (`llm_defaults`) | yes (with `set`); no (bare) | no |
| `kj backend reseed` | Restore the factory backends, model windows, aliases, and defaults to their embedded definitions. Operator-added backends are left alone | yes-destructive | no |
| `kj cast list` | List casts | no | no |
| `kj cast show` | Show one cast and its slots, with the defaults cascade applied | no | no |
| `kj cast create` | Create a cast (no slots yet) | yes | no |
| `kj cast remove` | Remove a cast; its slots cascade away with it | yes-destructive | no |
| `kj cast set` | Update a cast's mutable fields (today: description only) | yes | no |
| `kj cast slot set` | Create or replace one role's seat | yes | no |
| `kj cast slot remove` | Remove one role's seat | yes | no |
| `kj alias list` | List aliases | no | no |
| `kj alias set` | Create or re-point an alias | yes | no |
| `kj alias remove` | Remove an alias | yes | no |
| `kj cas put` | Ingest a file, print its hash | yes | no |
| `kj cas get` | Retrieve by hash. With `--out`, write the bytes to a file | no (writes only the `--out` file) | no |
| `kj cas ls` | List all stored objects | no | no |
| `kj cas info` | Show metadata (mime, size, path) for a hash | no | no |
| `kj cas rm` | Remove an object (unconditional, no ref-checking) | yes-destructive | no |
| `kj cc list` | List live Claude Code sessions from ~/.claude/sessions/*.json | no | no |
| `kj cc send` | Deliver a message into a live session's inbox over its messaging socket | yes (external side effect) | no — approval-ledger gated, cc.rs:110-118 |
| `kj ledger list` | List asks. By default, shows the live pending queue, oldest first | no | no |
| `kj ledger show` | Show one ask in full, including the statement being authorized | no | no |
| `kj ledger allow` | Allow one ask (claims it first; exactly one answerer wins) | yes-destructive (irreversible decision) | no |
| `kj ledger deny` | Deny one ask (claims it first; exactly one answerer wins) | yes-destructive (irreversible decision) | no |
| `kj ledger rules` | List active (not-yet-forgotten) standing rules, most recently created first | no | no |
| `kj ledger forget` | Forget a standing rule so its statement escalates to a human again | yes | no |
| `kj ledger runs` | List rc lifecycle runs, most recent first | no | no |
| `kj ledger signal` | Advisory signals — attached to an ask but never themselves a gate | yes (with `add`) | no |
| `kj db backup` | Hot-backup kernel.db to <path> via SQLite's native VACUUM INTO | yes (writes the target file) | no |
| `kj db checkpoint` | Flush the WAL into kernel.db and report whether it completed cleanly | yes | no |
| `kj audio beats` | Beat and downbeat tracking via the Beat This! (ISMIR 2024) model | no | no |
| `kj midi list` | List known device profiles (name + title pulled from the doc) | no | no |
| `kj midi show` | Print one device's profile document | no | no |
| `kj midi send` | Emit raw MIDI at a named device (the sink resolves the port) | yes (live hardware effect) | no |
| `kj midi identify` | Ask a device what it is and record the answer as a pulled fact in /run/midi/<device> | yes | no |
| `kj midi panic` | All-notes-off + all-sound-off on all 16 channels of <device> | yes (live hardware effect) | no |
| `kj roster status` | Post (or update) your own self-reported status | yes | no |
| `kj roster list` | List the current roster — who's around right now | no | no |
| `kj cp` | Copy a file between VFS paths via the streaming pump | yes-destructive (can clobber an existing dst) | no |
| `kj play` | Plays now over every attached client's render target; with `--track`, commits a clip record onto the score | yes (with `--track`); no (bare) | no |
| `kj rc add` | Install a script. `--content <body>` (or piped stdin) is the script text | yes | no |
| `kj rc list` | List installed scripts, optionally filtered | no | no |
| `kj rc rm` | Remove a script | yes-destructive | no |
| `kj rc show` | Print one script's content + metadata | no | no |
| `kj rc edit` | Edit a script. With `--content` it replaces the body; with no body it opens an interactive vi editor session | yes-destructive | no |
| `kj rc reset` | Restore one script to its embedded seed | yes-destructive | no |
| `kj rc reseed` | Install every path whose embedded seed never landed | yes-destructive | no |
| `kj editor open` | Open an editor on a path, binding to the kernel block that owns it | no | no |
| `kj editor keys` | Feed vim keys to a session | yes | no |
| `kj editor state` | Print a session's current buffer/cursor/mode/dirty state | no | no |
| `kj editor save` | Checkpoint the buffer as saved (`ZZ`); for a file, also flush to disk | yes | no |
| `kj editor quit` | Roll the block back to the last checkpoint and close the session (`ZQ`) | yes-destructive (discards uncommitted edits) | no |
| `kj editor list` | List open editor sessions (session, path, dirty, mode, opener) | no | no |
| `kj swap list` | List every unflushed file buffer the kernel is holding | no | no |
| `kj swap ack` | Keep the unsaved buffer for `path` and write it to disk | yes | no |
| `kj swap discard` | Drop the unsaved buffer for `path`; disk content wins on the next read | yes-destructive | no |
| `kj config list` | List the config files the kernel currently holds | no | no |
| `kj config show` | Print one config file's content | no | no |
| `kj config reset` | Restore a config file to its embedded default | yes-destructive | no |
| `kj block list` | List blocks in a context with optional filters | no | no |
| `kj block inspect` | Inspect a single block's metadata | no | no |
| `kj block count` | Count blocks matching filters | no | no |
| `kj block read` | Read a block's full content. Mirrors MCP `block_read` | no | no |
| `kj block cat` | One-step blob readback: resolve a block's payload and print or save it | no | no |
| `kj block original` | Read back the byte-exact original an ingest transform consumed for this block | no | no |
| `kj block reproject` | Re-run the CURRENT ingest parser over this block's stored original and re-emit its style spans | yes | no |
| `kj block append` | Append text to a block (streaming-friendly). Mirrors MCP `block_append` | yes | no |
| `kj block history` | Show creation + version info for a block. Mirrors MCP `block_history` | no | no |
| `kj block diff` | Unified line-by-line diff of block content against original text | no | no |
| `kj block status` | Set the status field on a block. Mirrors MCP `block_status` | yes | no |
| `kj block edit` | Edit a block via line-based operations. Single op per invocation | yes-destructive | no |
| `kj block create` | Create a new block in a context. Mirrors MCP `block_create` | yes | no |
| `kj binding show` | Show a context's binding (deny-by-default if unbound) | no | no |
| `kj binding allow` | Grant a capability (widen the loadout). Privileged/admin only | yes | no |
| `kj binding revoke` | Revoke a capability (narrow the loadout) | yes | no |
| `kj binding reset` | Clear the binding → deny-all (deny-by-default) | yes-destructive | no |
| `kj policy show` | Show an instance's current QoS policy (timeout, max result bytes, concurrency) | no | no |
| `kj policy set` | Update an instance's per-call QoS policy. Pass at least one flag | yes | no |
| `kj mcp list` | Show every configured server (mcp.toml) alongside what's actually registered on the broker, with health | no | no |
| `kj mcp reload` | Re-read mcp.toml and reconcile: add newly-configured servers, remove ones no longer configured | yes | no |
| `kj hook list` | List every persisted hook (optionally filtered to one phase) | no | no |
| `kj hook show` | Show the full detail of one hook entry | no | no |
| `kj hook remove` | Remove a hook by id. Idempotent — an unknown id is not an error | yes-destructive | no |
| `kj hook add` | Register a new hook entry. Idempotent on `hook_id` | yes | no |
| `kj search` | Regex search across block content | no | no |
| `kj doc list` | List all documents. Includes the File and Symlink kinds that `kj context list` hides | no | no |
| `kj doc tree` | Render a document's block DAG as ASCII tree | no | no |
| `kj doc create` | Create a new document. For Conversation kind, prefer `kj context create` | yes | no |
| `kj doc delete` | Delete a document and all its blocks. CASCADEs to drop the contexts row, oplog, snapshots — irreversible | yes-destructive | **yes** — doc.rs:433 |
| `kj attach` | Attach to an existing context and run its rc `attach` lifecycle | yes | no |
| `kj transport attach` | Attach a context to a track — the context announces itself. Arms **stopped** + OODA-armed | yes | no |
| `kj transport detach` | Detach a context from a track — unbind. The track persists with its remaining attachments | yes | no |
| `kj transport play` | Start/resume the track's clock | yes | no |
| `kj transport pause` | Hold the track's clock (freeze the playhead) | yes | no |
| `kj transport stop` | Stop the track's clock (MIDI idiom: stop = stop the clock only) | yes | no |
| `kj transport tempo` | Set the beat period from a BPM value | yes | no |
| `kj transport ooda` | Arm/disarm one attached context's OODA loop, without touching the clock | yes | no |
| `kj transport clock` | Switch the track's beat driver: `system` or `modeled` | yes | no |
| `kj transport rotate` | Set (or clear) the self-fork rotate cadence — the page-turn | yes | no |
| `kj transport delete` | Delete a track — a rename-aside **tombstone**, never a hard delete. REQUIRES `--track <name>` explicitly | yes-destructive | no |
| `kj transport list` | List every track with its live state. READ-ONLY (needs no `transport` capability) | no | no |
| `kj drift push` | Send content to a target context, delivered immediately | yes | no |
| `kj drift pull` | Pull + LLM-distill from a source context into the caller's context | yes | no |
| `kj drift merge` | Summarize this fork back into the parent context (or a given ctx) | yes | no |
| `kj drift flush` | Deliver all staged drifts | yes | no |
| `kj drift queue` | Show the staging queue (yields queue u64 ids) | no | no |
| `kj drift cancel` | Remove a staged drift before flush (pre-flush only) | yes | no |
| `kj drift history` | Show drift edges for a context (yields edge UUIDs) | no | no |
| `kj drift edge rm` | Remove a post-flush drift edge by its UUID (see `kj drift history`) | yes-destructive | no |
| `kj stage commit` | Transition from Staging to Live. Merges the staged child live (a cross-context write) | yes | no |
| `kj stage status` | Show staging state and block counts | no | no |
| `kj stage include` | Set excluded=false on a block | yes | no |
| `kj stage exclude` | Set excluded=true on a block | yes | no |
| `kj cache list` | List cache breakpoints on the active context | no | no |
| `kj cache add` | Add a cache breakpoint. `--target message` also needs `--index` | yes | no |
| `kj cache clear` | Clear all cache breakpoints on the active context | yes | no |
| `kj vfs snapshot` | Recursive snapshot listing with generation stamps | no | no |
| `kj vfs activity` | Per-directory activity totals since kernel boot | no | no |
| `kj diff` | Unified diff between two kernel-held versions of a file | no | no |
| `kj model` | Report the effective model for a context | no | no |
| `kj models` | List configured LLM providers, their models, and --model aliases | no | no |
| `kj kaish primer` | Print the composed agent-onboarding primer (model + operating contract + builtins) | no | no |
| `kj fork` | Fork the current context into a child | yes | no |
| `kj drive` | Clock one autonomous turn on a context | yes | no |
| `kj wait` | Wait for a context's turn to finish and report what it produced | no | no |
| `kj system status` | Report whether the kernel is quiesced, how many turns are in flight, and how many asks are waiting on a human | no | no |
| `kj system ps` | List the kernel's own processes: turns in flight and the child processes kaijutsu spawned, longest-running first | no | no |
| `kj system quiesce` | Stop the kernel from starting turns. Writes keep landing and turns already running finish | yes | no |
| `kj system resume` | Clear the quiesce flag so turns start again | yes | no |

### `kj system` is gated by authority, not by `--confirm`

The whole `system` noun — `status` and `ps` included — requires
`Capability::System`, checked once in `dispatch_system` before the subcommand
runs. `quiesce` mutates durable state (a singleton row) and is not
confirm-gated, which is deliberate: it is the verb an operator reaches for
during an incident, and a confirmation round trip is exactly the friction that
sends someone to `kill -9` instead. It is also cheaply reversible — `kj system
resume` clears it — which is the property `--confirm` exists to protect and
this verb does not need.

This is the clearest example in the corpus of the two mechanisms being
orthogonal: high authority, no confirmation, low regret.

## Where help text and code disagree

**`kj context demote` reaches the same archived state `kj context archive`
gates.** The help calls the ladder "not latched (each step is reversible except
the last)" and it is ungated at every step — but the last step is not
reversible in the sense the other steps are: `DemoteOutcome::Archived`
(context.rs:1899-1907) sets `ContextState::Archived`, the identical end state
`kj context archive` guards behind `--confirm` at context.rs:1719. Two doors to
one outcome, one locked. `kj context promote` is the documented resurrection
path back out, which is why the row is `yes-destructive` rather than
irrecoverable.

**`kj rc reseed`'s warning is not a gate.** The help says "Prints a unified
diff of every change; read it before overwriting for real, because an
overwritten path cannot be recovered afterward" — which reads like a two-step
or dry-run flow. It is not. The diff prints and the overwrite lands in the same
invocation; `--dry-run` is an opt-in flag, not the default (rc.rs:939), and
there is no `caller.confirmed` check anywhere in rc.rs.

**`kj backend reseed` clobbers more than its help admits.** The description
says "Operator-added backends are left alone," which is true of backend rows,
but `reseed_factory_backends` unconditionally calls
`db.set_llm_defaults(&factory_defaults())` (seed_backends.rs:244) — any
`kj backend default set` the operator made is overwritten with no warning in
the help text and no confirmation.

**`kj cas rm`'s "unconditional" is literal.** `cas_rm` (cas.rs:245-257) parses
the hash and calls `cas.remove` with no reference check and no gate, exactly as
the help says.

## Other findings worth a label

- **`kj drift edge rm` has no capability check at all.** The dispatcher's
  `require_cap` list (drift.rs:116-126) covers push/pull/merge/flush/cancel and
  omits `Edge`, so the one hard `DELETE FROM context_edges`
  (kernel_db.rs:5615-5622) in the drift surface runs unguarded. It destroys
  provenance, not delivered content.
- **`kj play` has no capability gate**, in either play-now or durable
  `--track` commit mode — the only write-capable verb audited without one.
- **`kj rc rm` is only partly recoverable.** `kj rc reset <path>` restores the
  *embedded seed*, not what was removed. A script that had diverged from its
  seed loses the divergence permanently; a no-seed user-authored script cannot
  be reset at all (rc.rs:1493-1516).
- **`kj transport delete`'s tombstone claim is true in code** —
  `tombstone_track` (kernel_db.rs:5211) renames the row and sets `deleted_at`;
  the hard-deleting `delete_track` exists but is never reached from this verb.
- **`kj ledger allow`/`deny` are irreversible decisions** that release or
  refuse a gated statement, and are themselves ungated by `--confirm` — the
  ledger claim mechanism (one answerer wins) is the only guard.
- **`kj cc send`** is gated, but by the approval ledger (`run_gate`,
  cc.rs:110-118), not by `--confirm`.
