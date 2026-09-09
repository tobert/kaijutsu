# Operating the live kernel

The facts a session needs to touch the running kernel on zorak without
breaking it. Measured values (sizes, counts) are examples from the day
they were written; re-run the command rather than trusting the number.
`docs/audio-daemon.md` covers the audio nodes; `docs/cc-peer.md` the hook
adapter; `docs/character.md` the bridge identity.

## The service

zorak runs the kernel as a systemd **user** unit from
`target/debug/kaijutsu-server`:

```
systemctl --user restart kaijutsu-server.service   # ~30 s to accepting
systemctl --user status kaijutsu-server.service
```

Its environment is the service's, not your shell's: an API key set in an
environment variable needs a restart, while `kj backend set` and the
config trees hot-reload.

## Before a bounce

```
kj system status        # turns in flight, asks pending
```

A bounce abandons every pending ask and cuts every turn in flight. A turn
that shows in flight with no `llm_stream` log lines for an hour is stale
and safe to cut. Amy's own client sessions are attached; when she has said
"you may bounce whenever today", bounce, otherwise ask.

## Deploy = build, back up, restart

```
cargo build -p kaijutsu-server
systemctl --user stop kaijutsu-server.service
cd ~/.local/share/kaijutsu/kernel
stamp="$(date +%Y%m%d-%H%M%S)"
for f in kernel.db kernel.db-wal kernel.db-shm; do
  cp -p "$f" "backups/$f.$stamp"
done
for f in auth.db auth.db-wal auth.db-shm; do
  cp -p "../$f" "backups/$f.$stamp"
done
systemctl --user start kaijutsu-server.service
```

Copy while stopped. Stopping does **not** checkpoint the WAL, so the
`-wal` file is part of the backup, not an optional extra. `auth.db` lives
one directory up, in `~/.local/share/kaijutsu/`, with its own `-wal` and
`-shm`; it is in the set since the keyring melt rewrites it. Chain the
copies so a missing file stops the script before the start, and start the
service by hand if it does. `backups/` is gitignored.
Snapshot retention is decided: the db grows, because `doc_snapshots.state`
is the conversation and a sweep there deletes sessions.

Schema changes in `kaijutsu.capnp` are additive by convention (interface
ordinals stay sequential, a retired method leaves a stub), so an old MCP
process keeps working across a kernel deploy.

## The MCP binary

`~/bin/kaijutsu-mcp` is a symlink into `target/debug`. Never copy over it.

```
cargo build -p kaijutsu-mcp     # this is the deploy
```

Every Claude Code session picks the new binary up on its next `/mcp`
reconnect. Rebuild it after any kaish or wire change. Hook events reach
the listener in the same process tree through the PPID-derived socket.

After a reboot the ssh-agent is empty and Claude Code reports the kaijutsu
server as "Connection closed" at startup:

```
ssh-add ~/.ssh/kaijutsu-lead    # no passphrase
```

then `/mcp`.

## Driving the kernel from Bash

There is no standalone `kj` binary. Drive `kaijutsu-mcp --connect` over
stdio: `initialize`, `initialized`, `register_session`, then `tools/call`.
A `register_session` with a label another live session holds conflicts;
pick a fresh label.

The lfm2d advisory gate escalates most `kj` writes from an MCP seat
(create, rename, retag, resume, rebind, and today `handoff note`). An
approval executes: `kj ledger allow <id>` runs the stored command in the
ask's context. Chain with `&&` inside one command; `;` trips the
shell-escape guard. A same-seat allow is refused, so an MCP seat needs a
second seat to approve it.

`kj context create --type <t>` is how to probe a type's rc. A reseed seeds
from the **binary**, so build first. Ephemeral test kernels blank every
S50 scorer.

## Parallel lanes in one working tree

Subagents make no git mutations. Each lane gets a disjoint file territory,
and the lead commits path-scoped:

```
git commit -F <msg-file> -- <paths>
```

`git add <paths> && git commit` commits the whole index, including a
sibling's staged work. Verify a lane's citations by re-reading each
file:line, and re-run its red-test mutations by hand.

A lane whose crate a sibling can transiently break may loop "waiting for a
notification" that does not exist. Tell it there is nothing to wait for,
and stop it if it loops again.

## Test runs

Never pipe a test run through `tail`; capture to a file and read that.
Foreground `sleep` is blocked in the Bash tool; `top -b -n 2 -d N` is the
wait. Bevy startup panics (B0001) do not surface in the unit suite because
tests never initialize schedules; launch the app to catch them.

## Machines

`hostname` says where you are. moltar's wallclock runs about 100 s behind
zorak's and NTP is off on both; the kernel's clock is the timebase and
nodes model their offset from the ping, so do not "fix" the hosts for our
sake. rc-driving threads need `KAISH_RC_THREAD_STACK` or the kernel
SIGABRTs.
