# `kj system` — stopping the kernel without killing it

The operator's surface for an instrument that is doing something you want
it to stop doing. It exists because the only lever that used to work was
`systemctl restart`, and that loses every turn in flight, every background
job, and any chance of looking at what went wrong.

Status: `kj system status`, `ps`, `quiesce` and `resume` ship. `seppuku` and
`stop` are designed here and unbuilt, as is the rc `shutdown` verb that
`quiesce` will call once it exists.

## The definition everything follows from

**Quiesced means the kernel accepts writes but does not react.**

Blocks still land. Ledger answers still land. Config edits still land.
Those are RPC → SQL → disk and inert on their own — nothing about a write
starts work. What stops is the kernel *acting* on them: turns.

This is narrower than "stop accepting input", which was the first design
and was wrong (Amy, 2026-08-24: *"I don't see why we need to close input.
If the user wants to type into blocks, those are almost direct rpc to sql
and disk transactions, and inert alone, the kernel just won't react while
quiesced."*). The narrower rule deletes a whole category of work: no
intake gating on writes, no refusal path for a compose keystroke, no
resume that has to reopen anything.

## The enforcement point

There is exactly one, and it is already load-bearing.

`spawn_llm_for_prompt` (`kaijutsu-server/src/llm_stream.rs`) calls
`mark_turn_begun` on its own stack before spawning the stream task. Its
own comment records why that covers everything: the autonomous path is
already marked by the time the function runs, so the mark there is a
harmless re-insert, and the two interactive callers (`prompt` /
`submit_input`) have no other mark. Every turn that reaches a provider
passes through it.

So quiesce is a check at that one call, not a sweep. Three production
sites mark a turn begun — `publish_turn_request`
(`kaijutsu-kernel/src/kj/fork.rs`), the gate-resume driver
(`kaijutsu-server/src/rpc.rs`), and `spawn_llm_for_prompt` — and only the
last one is downstream of all of them.

**Drivers keep running.** The turn driver, beat scheduler, editor
reconciler, and gate-resume driver all keep draining their buses while
quiesced; each turn start is refused with a legible error. Pausing them
instead would make the beat scheduler's musical timeline drift, and
`docs/midi.md` is explicit that we model a clock rather than chase one.

## The flag is durable, and that is the point

Quiesce sets a flag that survives a restart. The kernel comes back
quiesced and stays that way until someone enables it.

The first draft argued for in-memory, on the theory that a quiesce you
cannot clear by restarting is a way to brick the kernel. Amy's reasoning
is better: systemd restarts the kernel anyway, so **the quiesce/seppuku
cycle is a cheap restart** — and a durable flag is what turns it into an
*emergency stop that comes back inspectable*. Without the flag, seppuku
means the kernel returns and immediately resumes whatever was running away.

## The verbs

| verb | what it does |
|---|---|
| `kj system status` | What is running: turns in flight, asks waiting on a human. Read-only. |
| `kj system quiesce` | Set the flag. Run each live context's `shutdown` rc hooks. Stay up. |
| `kj system resume` | Clear the flag. |
| `kj system seppuku` | Set the flag and exit immediately. No hooks, no drain, cannot be cancelled. |

`seppuku` is not a joke verb with a real one hiding behind it — it is the
edge case handler, and its existence is a **scope reducer**. Because there
is always a way out, the graceful paths only have to handle the cases
where the logic is clear; everything fuzzy exits (Amy: *"we want graceful
shutdown where the logic makes sense, and for the edges of things, we
exit"*). Concretely it means we do not owe: a bounded-wait drain loop,
background-job reconciliation, or half-written editor session recovery.

The symmetry worth keeping: **quiesce runs the shutdown hooks; seppuku
skips them.** That is what makes seppuku honest rather than merely violent.

## rc owns the cancellation policy

Quiesce does not decide what to do about a turn already running. It runs
`/etc/rc/<context_type>/shutdown/SXX-*.kai` and lets rc decide, the same
way rc already owns stance, loadout, and scoring policy.

This is the right seam because the answer differs by what is running, and
the differences are judgment rather than mechanism (Amy, 2026-08-24):

- **local inference** — just exit; there is nothing to save.
- **batch requests** — leave them alone.
- **flash turns** — let it run; maybe we get it, maybe we do not.
- **frontier models** — cancel, to save cost and keep the KV cache and the
  conversation aligned.
- **or checkpoint** — an option, not yet a design.

A `shutdown` verb is a clean addition to rc's existing set (`create`,
`fork`, `drift`, `tick`, `rotate`) and reuses the same path shape.

**Deadlines are tight and non-negotiable.** rc's shutdown bucket fires
once per live context, so N contexts means N script runs at exit. A hook
that hangs is exactly what makes a person reach for `kill -9`, which
defeats the whole surface. When the deadline blows, the answer is seppuku
— never "wait longer".

## What rc needs that does not exist yet: a process table

A shutdown hook that cancels things has to be able to see them, and today
it cannot.

What exists:

- `BackgroundRegistry` (`kaijutsu-kernel/src/background_exec.rs`) tracks
  real OS pids, commands, status, and start/finish times. It already has
  `list_for_context`, the kernel-wide `summary_by_context`, `cancel`, and
  `kill_all_for_context`.
- An **MCP surface** on `builtin.background`:
  `list_background_processes`, `read_background_output`,
  `kill_background_process`.
- The turn-liveness registry, which `kj system status` now reads.

What is missing: **a `kj` verb**, which is what an rc script can actually
call. An rc hook speaks kaish and `kj`, not MCP tool names.

A `kj system ps` would union the two rosters that matter — turns in flight
(agent work, no pid) and background jobs (real processes, with pids) —
which together are the honest answer to "what is this kernel doing".

**On hooking up `kaish jobs` instead.** kaish already ships `jobs`, `ps`,
`kill`, `bg`, `fg`, and `wait`. Inside a kaijutsu shell, `ps` lists host
processes and `jobs` lists *kaish's* own jobs — not kaijutsu's
per-context background registry. Making `jobs` report the kaijutsu
registry would redefine an existing kaish public surface from inside an
embedder, and kaish is the conservative side of this fleet (CLAUDE.md,
"kaish and kaibo are not this"). Treat it as a question for the kaish
lead — is there an embedder hook for job sources? — rather than something
kaijutsu changes unilaterally. `kj system ps` does not need that answer to
ship.

## Who may call it

`kj system` is for the most privileged contexts only. It needs its own
capability rather than riding on an existing one, because the thing it
gates is not "may mutate state" but "may stop the instrument".

The distinction that matters, and the reason MCP earned its investment
(Amy, 2026-08-24): after a seppuku the kernel returns quiesced, and
**kaijutsu's own agents must not be able to bring it back**. A coder or a
musician running inside the kernel cannot resume it. A human can, and so
can an external agent over MCP — Claude Code is a player here, and always
will be. That asymmetry is the whole safety story: the instrument can be
stopped from outside and cannot restart itself.

Note this is a *capability* in kaijutsu's sense — an ergonomic nudge that
removes a footgun from seats that have no business holding it — not a
security control. Every player is still inside one trust boundary
(`docs/instrument-design.md`, "Many hands, one trust boundary"). The point
is that a runaway agent should not be able to un-stop itself by accident,
not that it is being defended against.

## `stop` — one mechanism, two scopes

`interruptContext` is not a separate idea. It is the single-context,
cancel-now cell of the same table quiesce lives in (Amy, 2026-08-24:
*"isn't it a form of quiesce with cancel targeted at one context now?"*).
Writing the table out:

| | refuse new work | cancel what is running |
|---|---|---|
| **one context** | (unbuilt) | `interruptContext` — exists, interactive |
| **whole kernel** | `quiesce` | `quiesce` + rc `shutdown` hooks, or `seppuku` |

So `kj system stop <target>` fills the per-context cancel cell with a
non-interactive door, and `<target>` can be either kind of row `ps`
prints — a context (cancel its turn, kill its jobs) or a single job id.

**Feasibility is split, and the split is a crate boundary.**

- **The job half works today, directly.** `BackgroundRegistry` lives in
  `kaijutsu-kernel`, so `kj` can call `cancel` and `kill_all_for_context`
  with no new plumbing.
- **The turn half cannot.** `ContextInterruptState` and `get_interrupt`
  live in **`kaijutsu-server`** (`interrupt.rs`, `rpc.rs`), and
  `kaijutsu-server` depends on `kaijutsu-kernel`, not the reverse. `kj`
  cannot call up.

That is the same wall `kj drive` hit, and its module docs already state
the resolution: *"The kernel can't call the server's turn driver directly.
It clocks a turn by publishing `TurnFlow::Requested` on the FlowBus."*
An interrupt request should take the same route — `kj` publishes, a
server-side subscriber calls `get_interrupt(...).soft()/.hard()`. Prefer
that over moving the interrupt registry down into the kernel: the bus hop
is the established pattern for exactly this direction, and it keeps the
`!Send` per-connection machinery where it already lives.

`interruptContext` stays regardless — it is the app's Ctrl+C path, which
is interactive and chatty.

## What this lets us retire — less than it looks

The rule that decides it is already written down (CLAUDE.md): *`kj` is good
enough for all admin-like stuff; normal ops over RPC is probably still
advisable for chatty paths.* Applied to the surfaces `ps` overlaps:

| surface | verdict |
|---|---|
| `list_background_processes` (MCP) | **Retirement candidate.** `kj system ps` and the bare `ps` builtin now render the same roster, and `ps` supplies the job ids the other two tools need. |
| `read_background_output` (MCP) | **Keep.** Polling a running job's output mid-turn is the definition of a chatty path. It is not admin work and must not become a `kj` verb. |
| `kill_background_process` (MCP) | **Judgment call.** Admin-shaped, but a model stopping its own runaway job mid-turn is ordinary operation, not administration. |
| `interruptContext` (RPC) | **Keep.** It has a real interactive caller — the app's Ctrl+C path (`kaijutsu-app/src/input/systems.rs`). Per-context and interactive; quiesce does not replace it. |

Caution before acting on the one candidate: these are **MCP tools, so their
callers are outside this repo** — external agents and the models in the
kernel. "No internal caller" is not evidence of disuse the way it would be
for an internal function. Retiring one is a roster change external clients
see.

## Who holds `kj system` — narrow seats hold none of it

An earlier draft here argued `ps` and `status` should stay open to every
seat, on the grounds that a seat wants to know whether something is
already running. Amy's read is narrower and it wins (2026-08-24): *"coder
doesn't really need to know, nor does a musician. they can always drift
questions to a help desk."*

That is the loadout doctrine applied consistently. A coder's job is its
task; the kernel's process table is an operator's concern, and a seat that
cannot see it will not reason about it. The escape hatch is drift — a
question routed to a seat that *does* hold the capability — rather than
widening every loadout to cover a rare need. The help desk that answers
those is future work, with the janitors.

So `kj system` in full — `ps`, `status`, and the stopping verbs — belongs
to operator-shaped seats. Within it, `resume` still carries the extra
rule: kaijutsu's own agents may never call it, whatever else they hold.

**This does not make the `ps` shadow pointless — it is what makes it
safe.** `ToolRegistry` has no `remove`, so a kaijutsu shell cannot simply
lack `ps`; without the shadow a coder would get kaish's **host** process
table, which is strictly worse than getting ours. For a narrow seat the
shadow should render a refusal that names the alternative (drift a
question) rather than the roster. The `privileged` flag already threaded
through `kj/context_shell.rs` is the switch.

## Settled while building

**Quiesce refuses new turns and does not cancel running ones** (Amy,
2026-08-25). Cancellation is rc's `shutdown` policy, where it can differ by
model tier; keeping it out of quiesce meant the flag touched no in-flight
machinery at all. The consequence to be honest about: quiesce alone would not
have stopped the 14 concurrent turns of 2026-08-24 — it stops the 15th.
Stopping the running ones needs the rc `shutdown` verb, or seppuku.

**The flag lives in `kernel_db`**, in its own singleton table
(`system_quiesce`). Absence of the row is the running state, so a fresh
database runs and `resume` is a `DELETE`. Every read goes to disk: the flag is
set precisely when something has gone wrong, and a cached "running" would
start the turn the operator was stopping.

**A read failure refuses the turn.** A kernel that cannot answer "am I
stopped?" is not one to start a turn on.

**`status` reports the flag first**, because it changes what the other numbers
mean — turns in flight on a quiesced kernel are the ones finishing, not the
ones starting.

## Open

- `quiesce` does not yet run the `shutdown` rc hooks; that arrives with the rc
  verb. Until then it is purely the flag.
- `seppuku` and `stop` remain unbuilt. `stop` still needs the FlowBus hop for
  the turn half — `ContextInterruptState` lives in `kaijutsu-server`.
