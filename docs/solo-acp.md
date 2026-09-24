# kaijutsu-solo-acp

One command that is a whole kaijutsu. `kaijutsu-solo-acp` starts a private
kernel inside its own process and serves ACP v1 on stdio, so an ACP client —
an editor, or a benchmark harness that launches one command inside a task
container — gets a working agent without anyone standing up a server first.

`kaijutsu-acp` is the bridge to a kernel somebody else runs. This binary is
that bridge plus the kernel, and the startup a person would otherwise do by
hand: a state directory, keys, a root identity, config trees, model defaults,
and the performer character a model turn needs.

The wire between the two halves is the real one. The kernel listens on
`127.0.0.1` on a port the operating system chooses, and the bridge dials it
over SSH and Cap'n Proto with the key this process generated. There is no
in-process shortcut, so anything that works here works against a shared
kernel.

## Running it

```bash
# with exactly one provider key in the environment
kaijutsu-solo-acp

# naming the provider, the model, and a state directory to keep
kaijutsu-solo-acp --backend-kind deepseek --model deepseek-v4-flash \
  --state-dir ~/solo-kernel
```

stdout is the ACP wire. Every diagnostic goes to stderr, including the one
line naming the state directory. `RUST_LOG` sets the level.

As an ACP registry entry — the `agent.json` Harbor reads, with a `local`
distribution (`src/harbor/agents/installed/acp.py`, `AcpRegistryEntry` and
`AcpLocalTarget`):

```json
{
  "id": "kaijutsu-solo-acp",
  "name": "kaijutsu",
  "version": "git-<short-rev>",
  "description": "kaijutsu with its own kernel, over ACP v1",
  "distribution": {
    "local": {
      "cmd": "/usr/local/bin/kaijutsu-solo-acp",
      "args": ["--backend-kind", "deepseek", "--character", "solo-coder"]
    }
  }
}
```

`id` becomes the agent's name in every benchmark result, so it must be
`kaijutsu-solo-acp` — not a shorter alias. `version` should identify the
build that ran, not a static number: `contrib/bench/harbor/`'s working
adapter derives it from the git revision this binary was built at
(`git-<short head>`, plus `-dirty` for an uncommitted tree), and that is the
working example to copy rather than hand-writing a version string here.

The provider key rides the environment the runner launches with, or the key
file the backend row names. It never belongs in this file.

## Flags

| Flag | What it decides |
|---|---|
| `--state-dir <dir>` | Where the databases, keys, and `/config` live. Default: a temporary directory, removed however the process exits. A named one is never removed. |
| `--backend-kind <anthropic\|deepseek\|openai>` | Which provider. Default: the one whose key is in the environment, when exactly one is. |
| `--base-url <url>` | An OpenAI-compatible endpoint of your own. |
| `--model <id>` | The model every turn uses. Default: `deepseek-v4-flash` or `claude-sonnet-5`; the `openai` provider ships no model id, so it needs this flag. |
| `--api-key-env <VAR>` | The variable holding the key. The key itself is never a command-line argument. |
| `--character <name>` | The performer each session's context is played by. Default `solo-coder`, created if absent. |
| `--context-type <type>` | The rc bundle each session's context runs. Default `coder`. |
| `--mount <dir>` | Mount a host directory read-write at the same path inside the kernel, so the model's file tools may write there. Repeatable. |
| `--no-cwd-mount` | Do not mount the directory the agent was launched in. |
| `--gate-config <file>` | A gate policy to install verbatim, replacing the shipped default. |
| `--consent <collaborative\|autonomous>` | The consent mode every session this kernel serves runs in. The agentic tool-loop has no per-turn iteration cap; nothing in the kernel reads this mode today (`docs/issues.md`, "Consent setting ownership"). Default: the kernel's own default, collaborative — plain use is unchanged. |
| `--max-tokens <N>` | The output token ceiling written into this kernel's model defaults. Must be greater than zero; zero and negative values refuse the start. A value above the provider's own per-model ceiling is rejected by the provider, not by this flag. Default: the factory ceiling, 16384. |
| `--rc-overlay <dir>` | An rc variant to install over the seeded `/config/rc` tree, for A/B instruction sets (`contrib/bench/rc-variants/*/README.md`). The directory mirrors the rc tree's layout and holds only the files that differ; every regular file under it (a top-level `README.md` is skipped) replaces the file at the same relative path, unlinked first so a seeded symlink into `lib/` is replaced rather than written through. Refuses, before anything is replaced, if the directory is missing or empty, contains a symlink or another non-regular file, or names a file whose seeded parent directory does not exist. Idempotent: re-applying the same overlay against a persistent `--state-dir` is safe. |

Zero flags works when exactly one of `DEEPSEEK_API_KEY`, `ANTHROPIC_API_KEY`,
or `OPENAI_API_KEY` is set. Zero keys, or several, is a refusal naming what to
set: guessing would send a first prompt to a provider nobody chose. A named
provider may take its key from the file the factory backend row names
(`~/.deepseek-key` and the like) instead of the environment.

## The key and /proc

The provider key lives in this process's own environment, so a process the
model spawns, running as the same uid, could otherwise read it straight out
of `/proc/<this pid>/environ`. This binary clears its dumpable flag
(`prctl(PR_SET_DUMPABLE, 0)`) at the very start of `main`, before any thread
starts, which makes `/proc/<pid>/environ` (and `mem`, and `maps`) owned by
root and unreadable to a same-uid reader. The process exits with a
non-zero status if that call fails, rather than continuing with the key
readable.

Two limits. It is not a defense against root: a benchmark task container
commonly runs the agent's own process as root, where a same-uid process is
root too and the key stays readable regardless — there the defense is a
run-scoped key in a disposable container, not this mitigation. It also
disables core dumps and same-uid `ptrace` of this process, as a side effect
of the same flag.

## The workspace

A model can write only where the kernel mounts a directory read-write
(`docs/mounts.md`). `$HOME/src` and `/tmp` always are; everything else under
`/` is read-only.

So this binary mounts the directory it was launched in, read-write, at its own
path. That is what an ACP client means when it starts an agent in a project
and passes that path as the session's cwd. The mount is skipped, with a line
on stderr saying why, when the launch directory is already writable, is `/`,
or cannot be mounted — a convenience that cannot be honored says so and the
kernel still starts. `--no-cwd-mount` turns it off.

`--mount <dir>` names another directory, and it is a request rather than a
convenience: a directory that does not exist, is not absolute, or sits on a
reserved kernel path refuses the start instead of being skipped.

```bash
# Harbor-style: the task directory is the workspace
cd /app && kaijutsu-solo-acp

# or name it
kaijutsu-solo-acp --mount /app --no-cwd-mount
```

## Keeping a state directory

A `--state-dir` keeps the kernel: its contexts, transcripts, characters, and
settings are all there on the next run, and the connecting key is reused
rather than regenerated. Two things follow.

The model defaults are rewritten at every boot, so a second run with a
different `--backend-kind` or `--model` changes what NEW sessions use. An
existing context keeps the provider and model stamped on it, and `kj model`
inside that session is what changes it.

The root character is written once. A state directory that already belongs to
a different live root refuses, loudly, rather than adding a second one — so a
directory another kernel owns is not somewhere to point this.

## Startup order

Each step fails loudly, with what it was doing:

1. Resolve the provider and model. This runs first, so a kernel with nowhere
   to send a prompt refuses before it writes anything.
2. Resolve the state directory — the named one, or a fresh temporary one.
3. Load or generate the connecting character's Ed25519 key.
4. Make `solo` the root character and bind that key to it. `solo` owns the
   bridge's connection and is the reviewer every approval resolves to.
5. Seed the factory backends, then point the model defaults at the chosen
   provider and model. The factory row is left alone unless `--base-url` or
   `--api-key-env` says something different, which is what keeps a provider's
   key file working. `--max-tokens` overrides the factory output-token
   ceiling in this same defaults row; left out, the factory ceiling stands.
6. Create the performer character. A model turn needs a live performer
   distinct from its reviewer (`docs/approval-identity.md`), so `solo` reviews
   and `solo-coder` performs.
7. Resolve the read-write mounts: `--mount`, plus the launch directory unless
   it was declined or is already writable.
8. Start the kernel on `127.0.0.1:0`, on its own thread with the server's own
   stack size — boot runs the root context's rc create chain, and a smaller
   stack overflows it. The kernel seeds its `/config` trees on the way up,
   exactly as a fresh install does, and refuses to start on a mount it cannot
   honor.
9. Install `--gate-config`, if given.
10. Apply `--rc-overlay`, if given, to the seeded `/config/rc` tree. This
    must run after step 8 seeds it and before any context can be created —
    rc scripts are read fresh from disk at every lifecycle run
    (`kaijutsu_kernel::rc::mod::load_scripts`), not cached at boot, so
    landing here, before the ACP bridge connects, is early enough for every
    session this process serves.
11. Apply `--consent`, if given, to the running kernel — before the ACP
    bridge connects, so it is in place for every session this process
    serves. This is the one place a solo kernel's consent mode is set, though
    nothing in the kernel reads it today (`docs/issues.md`, "Consent setting
    ownership").
12. Connect the ACP bridge over the loopback wire and serve stdio.

On stdin EOF the kernel settles accepted work and checkpoints its database
before a temporary state directory is removed. If the kernel stops, fails, or
panics while a client is connected, the process says so on stderr and exits
non-zero rather than hanging.

A temporary state directory holds a generated private key, so its removal
does not depend on reaching the end of the program. It is registered for
removal at exit, which covers the orderly finish, a `SIGTERM` or `SIGINT` the
kernel's own handler answers after settling, and a kernel that dies under a
live client. A `--state-dir` you named is never removed.

## What it deliberately does not do

- **It does not widen the perimeter after boot.** The kernel freezes its
  mount table once it is up, so every mount is a launch decision and no verb
  adds one to a running kernel. Restart with another `--mount`.
- **It does not take a key on the command line.** Only the name of the
  variable holding one.
- **It does not fall back on the operator's kernel.** Every path —
  databases, host key, `/config` — is named explicitly, so nothing here
  defaults to `~/.config/kaijutsu` or `~/.local/share/kaijutsu`, and a
  `--state-dir` inside either is refused. What it does read is the provider
  key file a factory backend row names, which is the point of it. A
  `--state-dir` you name is used exactly as given, so pointing two solo runs
  at one directory points them at one kernel.
- **It does not configure casts, aliases, or rc.** A solo kernel runs one
  model on the shipped rc. Use `kj` inside a session, or a state directory
  that persists, for anything more.
