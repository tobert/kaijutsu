# `kj system` — stopping the kernel without killing it

The operator's surface for an instrument that is doing something you want
it to stop doing. It exists because the only lever that used to work was
`systemctl restart`, and that loses every turn in flight, every background
job, and any chance of looking at what went wrong.

Status: `kj system status` ships. `quiesce`, `resume`, and `seppuku` are
designed here and unbuilt.

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

## Open

- Should `quiesce` also cancel turns already running, or only refuse new
  ones? Leaning: refuse new ones only, and let rc's shutdown hooks decide
  about the running ones. That keeps quiesce non-destructive and gives
  seppuku a clear job — but it means quiesce alone would not have stopped
  the 14 concurrent turns of 2026-08-24.
- Where does the durable flag live? `kernel_db` is the obvious home; it
  must be readable at boot before any driver spawns.
- Does `status` report the flag? It should, or you cannot tell a quiet
  kernel from a stopped one.
