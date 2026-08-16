# 会術 Kaijutsu

Kaijutsu is a cybernetic system for multi-user multi-model multi-context collaboration.
It is an **instrument you play, not a harness that drives you** — you play it, a model
plays it, anyone with a connected app plays it too; many hands on one keyboard. The
kernel is the instrument's *body*: it holds context data, model interactions, workspaces,
and tools, and supplies what a turn needs without playing the turn itself. It speaks SSH
with Cap'n Proto over channels. (Named for humans in `docs/instrument-design.md`;
embodied — never preached — in the model-facing rc stances.)

## Stance

The kernel restates the cybernetic / 改善 / TDD posture in its own rc lifecycle:
`/etc/rc/coder/create/S00-stance.kai` reaches the model via the system-prompt slot for
every context with `context_type=coder`. rc scripts at `/etc/rc` are **kernel-owned** —
one owner, no host file, no write-through; embedded defaults under
`assets/defaults/rc/` seed a fresh kernel once. There is no host file to
`vim`: edit a live script with `kj rc edit <path> --content <body>`, and `kj rc reset
<path>` restores one to its embedded default. Change the shipped default by editing
`assets/defaults/rc/` (the in-repo seed). See `docs/config-crdt-ownership.md`.

**Config is different from rc, as of 2026-08-15: just write the file.**
`/etc/config`, `/etc/client` and `/etc/midi` are ordinary write surfaces for
the file tools and the editor — there is no `kj config set`/`edit` and no
`config-write` capability on them. `kj config` keeps only what has no file-tool
equivalent: `list`, `show`, and `reset` (restore the embedded default). `/etc/rc`
stays gated by `rc-write` because rc is executable rather than data.

*Migration in flight:* rc/config are still stored as **kernel documents** in
`kaijutsu-crdt`'s block store — not host files, and no longer a CRDT either
(block text became a plain `String` on 2026-08-16). Melting them to real files
on disk is open work. **Single kernel ownership is the invariant that survives
the change** — whatever the storage, config must never have two competing
sources of truth.

**Permission to get simpler** (Amy, 2026-08-15): *"If the agent can see the
files and edit them, that's fine, we don't need to complicate it just because
it's config."* Config is not a special category deserving its own machinery. If
plain files plus the tools every player already has (kaish, the file tools, the
editor) do the job, that is the answer — a reseed tool for the shipped defaults,
and git as a skill reachable through rc or the help system rather than something
the kernel performs. Prefer deleting a mechanism to generalizing it. This is
explicit permission to reduce scope, not merely to avoid adding to it.

**Shared trust, crosstalk-as-feature.** Every player — human, model, connected
app, sibling context — is *inside* the trust boundary; the kernel runs as one
unix user and the real boundaries live outside it. We design for resilience to
boundary trespass, not enforcement between cooperating players: crosstalk is a
feature (your neighbor's wrong note is one you cover). Capabilities/loadouts are
**ergonomic nudges for focus and mistake-prevention, not security** — "less
privileged" means *narrower focus* (footguns absent by construction), never less
trusted, and mistake-prevention is routed through the loadout, not through auth
denials between players. Full reasoning: `docs/instrument-design.md` ("Many hands,
one trust boundary") + `docs/chameleon.md`.

**Host exec has one owner.** All host process execution routes through
kaish — `EmbeddedKaish`'s `ExternalExec::Allow{path}|Deny` policy (set in
`kj/context_shell.rs`, enum in `runtime/embedded_kaish.rs`) is the one place
exec authority, ignore config, output limits, and VFS cwd resolution live.
The sole sanctioned exception is MCP stdio server launches
(`mcp/servers/external.rs`) — config-driven, spawned by `rmcp`, never
agent-supplied. A new ad-hoc exec site (another `/bin/sh -c`, another bare
`Command::new`) is a design conversation, not a patch: it re-derives policy
kaish already owns, and the copy drifts — see `docs/issues.md` for one that
did and is being retired.

## Durable state and the wire

**The kernel is the sole sequencer.** Kaijutsu is not a partition-tolerant
peer-to-peer system and does not try to be: there is one authoritative kernel,
every accepted mutation gets a kernel-assigned sequence, and gap recovery is
"ask the kernel again" — never a peer merge. Contexts are multi-writer because
many players share one kernel, not because replicas reconcile.

The shape every path should follow:

```text
rich RPC command  →  kernel validates and sequences  →  semantic operation +
materialized state commit atomically  →  projected event stream  →  thin clients
```

Three rules that follow, and they are the ones to check a patch against:

1. **Commands express intent; events express accepted facts.** Commit first,
   publish second — never publish an event you have not durably accepted.
2. **Clients never author or decode storage-engine operations.** A client that
   decodes an oplog to learn what happened is reaching around the contract. Ask
   for blocks and revisions as projected facts.
3. **The durable and wire vocabulary is Kaijutsu's domain vocabulary** — blocks
   authored, edited, completed, excluded; input edited, submitted, cleared — not
   a text engine's encoding of them.

**Clients read over the wire and write through kaish.** There is deliberately no
client-facing RPC for editing block text — `pushOps` was the only one and it is
deleted. A client *follows* a context through the change feed
(`docs/change-feed.md`) and *mutates* by asking the kernel to run something:
`kj block append`/`edit`, an MCP block tool, a kaish script. One mutation path,
one set of capability checks, instead of a parallel RPC surface that would drift
from it (Amy, 2026-08-15: *"clients should rely on stuff in kaish anyways most of
the time"*). `authorBlock` stays — authoring a whole block is a submission, not
an edit.

**And the line that decides where a new surface goes** (Amy, 2026-08-15): *"kj is
good enough for all admin-like stuff. normal ops over RPC is probably still
advisable for chatty paths."* Administration — config, rc, reset, reload,
anything an operator does occasionally and deliberately — is a `kj` verb, and
does **not** earn a wire method. Chatty paths that run at interaction rate —
compose keystrokes, the change feed, block queries — stay on RPC, because
routing them through a shell dispatch per event is a cost with no benefit. Three
config RPCs were deleted under this rule and had zero callers; `getConfig`
survived it, because a client fetching its theme at bootstrap step 0 has no
context to run `kj` in, and reading over the wire is what the rule above already
sanctions.

**The CRDT is gone — block text is a `String`** (2026-08-16).
`diamond-types-extended` is not a dependency of any crate and does not appear
in `Cargo.lock`; the wire carries no storage-engine operations; the compose
draft is an ordinary block. `kaijutsu-crdt` keeps its name and nothing else —
it is a block store now, and renaming it is open work rather than a pending
migration.

Two consequences that outlive the migration, because they are the reason it
was possible at all:

- **Concurrent merge into kernel documents is structurally impossible**, not
  merely unobserved. Deleting the `pushOps` handler removed `merge_ops`'s only
  concurrent caller, and replay is sequential self-application. Code that
  reasons about conflict resolution here is reasoning about a state the system
  cannot reach — check before building for it.
- **Do not reintroduce a text CRDT** for block content. Streaming is 100%
  append and `push_str` is amortized O(1), while per-block merge metadata cost
  a measured ~4x the text it represented. If a surface genuinely needs
  concurrent text merge, that is a design conversation.

Amy's ruling and the reasoning that started it: `docs/crdt-position-2026-08.md`.

## Crates

`kaijutsu-types` first — the shared types every other crate depends on. Then
`kaijutsu-crdt` (BlockStore; the block text engine, being demoted to a private
implementation detail), `kaijutsu-kernel` (Kernel, VFS, MCP broker,
LLM, drift, `kj` builtin), `kaijutsu-server` (SSH server, EmbeddedKaish),
`kaijutsu-client` (RPC client, Send+Sync ActorHandle), `kaijutsu-app` (Bevy 0.19 GUI;
inline SVG + ABC→staff rendering). Others: abc, audio, mcp, cas, agent-tools, editor,
index, telemetry, hyoushigi, viz. Wire schema: `kaijutsu.capnp`. The stdio MCP server (`kaijutsu-mcp`)
exposes most kernel capabilities and can be called as a hook from client applications.

## Time

Musical time is doctrine, not folklore — `docs/midi.md` "The one timebase":
never chase a clock (model it); wire timing artifacts carry emission wallclock
stamps and sinks back-date; stale timing data is rejected on a ladder; missed
beats are missed (never replay); the kernel grid is scheduled-periodic; a
dialed-in phasor free-runs on the local clock inside a deadband. Touch any
beat/clock/cue code with that section open.

## Conversation vs Context

**Context** is the durable side: the kernel-sequenced block log, exclusions, edits, conversation metadata. Multi-writer — many players, one kernel. Holds more than the live conversation knows about.

**Conversation** is the live session: an append-only message sequence shipped to the LLM. Hydrated from context once at boundary events (fork, new, cold start, attach) and append-only thereafter.

`block exclude` / `block edit` operate on the context and only take effect at the next hydrate boundary — typically fork. To remediate a poisoned conversation (giant tool output, bad turn): exclude in context, then fork. Async events between turns (shell output, drift, MCP calls from sibling agents) queue in a per-context mailbox and flush on the next turn. The mailbox is also the atomicity gate that keeps tool_use+tool_result pairs (and other must-travel-together blocks) from being split by unrelated writers.

## Machines

Kaijutsu is hacked on across three machines — `hostname` tells you where you are:

- **moltar** — Amy's office PC / gaming rig (Arch, real GPU). She is often at its
  controls; the eurorack is connected here and VR is coming. Live GUI runs,
  BRP-driven testing, and heavy builds belong here.
- **zorak** — AMD Strix Halo; increasingly the kaijutsu *server and inference*
  machine. Prefer not to burden it with builds.
- **Amy's work MacBook Pro** — macOS client, already works via Bevy. Mac support
  is expected: no Linux-only assumptions in the app without a mac story.

## Autonomous Development Loop

Most testing happens on a Linux server with a real GPU that the user can connect to with remote desktop.

```bash
# user starts this in the Wayland session:
./contrib/kaijutsu-runner.sh

# agents use:
./contrib/kj status|tail|pause|resume|rebuild|restart
```

The Bevy BRP tools work directly. Take screenshots frequently.

## Working Notes

Three markdown files carry work between sessions; keep them current **as you
go**, not at the end. They compete for context tokens in every future session,
so compression is part of maintaining them — the day-to-day detail is always
recoverable from each file's own git history.

- **`signoff.md`** (repo root, ephemeral, never committed) — the living handoff
  a fresh process can't reconstruct: where we are, next moves, live-environment
  facts, parallel-work warnings. Keep it to a couple screenfuls; melt durable
  parts into the repo docs before they go stale, and delete sections once
  melted. It is short-term memory, not an archive.
- **`docs/issues.md`** (committed) — the open-work backlog and side-quest valve. Record
  out-of-scope work here before moving on; **delete an entry when it ships** (melt the
  story into the devlog if it's worth keeping). Code is truth; this tracks what's *not*
  in the code yet.
- **`docs/devlog.md`** (committed) — the evolving narrative of how kaijutsu and
  its ideas took shape: arcs, decisions, and lessons, written oldest → newest.
  It is a story, not a standup log. Fold new work into the chapter it belongs
  to (or open one for a genuinely new arc) and compress chapters as they cool;
  prefer rewriting a chapter over appending another status update to it. Commit
  hashes, test counts, and daily blow-by-blow belong in `git log`, not here.

## Git Conventions

- Working on main (early development); parallel work on the same repo is common
- Add files by name, avoid wildcards; ephemeral markdown is usually not committed
- Set Co-Authored-By in commit messages, crediting the model that did the work.
- **Never run `cargo fmt`.** `rustfmt.toml` disables it repo-wide so the command
  is a no-op — don't work around that, and don't reformat by hand either. Match
  the surrounding style. Rationale + the condition that would reverse it:
  README.md "Code Style".

Commit and pull request bodies should usually summarize the decisions behind the
change, **drawn from the conversation with the user**. Commit messages briefly explain
what happened as context for the more important task of explaining the decisions we
made.

## App Input

All keyboard/gamepad/mouse input in `kaijutsu-app` flows through the central
action table (`crates/kaijutsu-app/src/input/`): raw input → Ctrl+A prefix →
one dispatcher → `ActionFired` → domain handlers. **Never read
`ButtonInput`/`KeyboardInput` directly in a view or scene** — add an
`InputContext` + bindings to the table instead; gamepad, `bindings.toml`
rebinding, and the `?` legend then come free. The vi editor is the one
sanctioned raw reader (an explicit keyboard grab). `docs/input.md` is
canonical: Esc doctrine (vi owns Esc where a vi surface is live; elsewhere
it is exactly one `PopLevel`), the Ctrl+A prefix table, clipboard model.

## Bevy Quick Reference

**We are on Bevy 0.19** (`Cargo.lock`, and `crates/kaijutsu-app/Cargo.toml`
declares `"0.19"`). This heading said 0.18 until 2026-08-15, when a subagent
reported the mismatch mid-task — the doc warning below about a stale checkout
applies to this document too. **The lockfile is the answer to "what are we on";
check it rather than this line.**

Trust this table over training memory — these renames landed in 0.18, still
hold in 0.19, and are newer than most model training.

| Old (0.14-0.17) | New (0.18) |
|-----------------|------------|
| `#[derive(Event)]` | `#[derive(Message)]` |
| `EventReader<T>` / `EventWriter<T>` | `MessageReader<T>` / `MessageWriter<T>` |
| `events.send(x)` | `messages.write(x)` |
| `app.add_event::<T>()` | `app.add_message::<T>()` |
| `ChildBuilder` | `ChildSpawnerCommands` |
| `BorderColor(color)` | `BorderColor::all(color)` |
| `query.get_single()` | `query.single()` |

**HiDPI units:** `ComputedNode` (size/content_box/border/padding) and UI
`GlobalTransform` are **physical** pixels; font sizes, `Val::Px`, and
`ScrollPosition` are **logical**. Never feed a raw `ComputedNode` dimension
into layout math — convert via `view::ui_rtt::logical_size` /
`logical_content_size` (invisible at 1x, breaks on HiDPI).

Bevy source: `~/src/bevy`, examples at `~/src/bevy/examples/`

**Check what `~/src/bevy` is on before trusting it.** It is a working
checkout, not a pinned mirror — on 2026-08-12 it was five months stale
(0.18.1) while 0.19.0 was out, so a sweep planned against it would have
concluded 0.19 did not exist. Run `git -C ~/src/bevy describe --tags` first,
`git fetch` and check out the tag you actually mean, and **say in your
findings which tag you read**. While we are mid-migration, **prefer the cargo
caches** (`~/.cargo/registry/`) as truth for "what does version X require" — a
cached `.crate` tarball's `Cargo.toml` is the real manifest and cannot go
stale the way a checkout can. This applies to subagents too: tell them the
tag, don't let them assume.

A related trap that already bit us: **a dependency's own version number says
nothing about which Bevy it targets.** `bevy_brp_extras = "0.19"` required
`bevy 0.18.1`; its first release wanting Bevy 0.19 was 0.21.0, and the
workspace now pins `0.22`. Read the pin, never infer it from the number.
