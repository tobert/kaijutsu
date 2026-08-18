# kj — kernel command interface

Manages contexts, drift, forks, presets, and workspaces in a kaijutsu kernel.

## Context References

Commands accept a context reference wherever `<ctx>` appears:

- `.` — current context (the default when no ref is given)
- `.parent` — the context this was forked from (chainable: `.parent.parent`)
- `explore` — label match (exact, then unique prefix)
- `019c779b` — hex prefix of context UUID

## Common Workflows

### Parallel Exploration

```bash
# Fork two approaches — you stay on the parent (POSIX fork semantics);
# --prompt drives each child's first autonomous turn
kj fork --name approach-a --prompt "try X"
kj fork --name approach-b --prompt "try Y"

# Pull findings back into the parent
kj drift pull approach-a "summarize what you tried"
kj drift pull approach-b "summarize what you tried"
```

### Share a Finding

```bash
# Send a concrete finding (no LLM, fast — delivered immediately)
kj drift push main "retry logic in client.rs:142 drops errors silently"
```

### Complete a Fork

```bash
# Summarize this fork's work back to parent (LLM distillation)
kj drift merge
```

## Commands

```
alias           list, set, remove — short --model handles → backend/model
                (the kernel ships none; they're yours to define)
attach          Attach to an existing context and run its rc attach lifecycle
                (distinct from `transport attach`, which attaches to a beat track)
audio           beats — offline audio analysis (beat/downbeat tracking via beat-this)
backend         list, show, set, remove, model set|remove, default show|set, reseed —
                SQL-native LLM endpoints (name + kind), their context windows,
                and the kernel-wide defaults. Replaces models.toml entirely.
binding         show, allow, revoke, reset — a context's tool-capability allow-set
                (cap tokens incl. rc-write, config-write, drive, fork, drift, transport,
                operator, exec, admin, or <instance>[:<tool>], facade:<name>, *, facade:*)
block           list, inspect, count, read, cat, append, history, diff, status, create,
                edit (insert|delete|replace)
cache           list, add, clear — Claude prompt-cache breakpoints on the active context
cas             put, get, ls, info, rm — content-addressed blob storage
cast            list, show, create, remove, set, slot set|remove — named model
                ensembles (role → backend/model + tunables); role is a
                context_type; `set --desc` edits the description after create
                ("" clears it to NULL)
cc              list — roster of live Claude Code sessions on this machine,
                read from ~/.claude/sessions/*.json (never reads *.key files);
                send — deliver a message into a live session's inbox, gated
                behind the approval ledger (`--dry-run` exempt)
config          list, show, set, edit, reset — CRDT-owned config at /etc/config
                (system.md, theme.toml, mcp.toml) + per-client at /etc/client
context (ctx)   list, info, current, switch, create, scratch, set, unset, log, move,
                rename, archive, conclude, promote, demote, pause, resume, remove,
                retag, hydrate
cp              Copy a file between VFS paths via the streaming pump (-r not implemented)
db              backup <path>, checkpoint — hot SQLite backup (VACUUM INTO, absolute
                path required) and WAL checkpoint/quiesce; restore is deliberately NOT
                a verb (see `kj db backup help`)
diff            <a> [<b>] — unified diff as a typed block: one path diffs disk
                against the CRDT document that owns it, two paths diff both
                documents, --from/--to address a document's journalled history
doc             list, tree, create, delete — storage layer (all kinds, not just conversation)
drift           push, pull, merge, flush, queue, cancel, history, edge rm
drive           Clock one autonomous turn on a context (--prompt)
editor          open, keys, state, save, quit, list — kernel-owned vi editor sessions
fork            Fork current context (--name, --prompt, --preset, --model,
                --include/--exclude ranges, --compact, --as, --stage, --switch)
hook            list, show, remove, add — broker hook tables, direct (never
                through hook evaluation); the recovery path for a self-inflicted
                PreCall Deny("*") lockout
kaish           primer — composed kaish agent-onboarding guidance (kaish-help);
                what S05-kaish.kai turns into a per-context system block
ledger          list, show, allow, deny, rules, forget, runs — answer pending
                approval-ledger asks left by gated verbs (e.g. `kj cc send`);
                allow/deny take `--remember <session|always>` to generalize
                the decision into a standing rule (refused for `allow` when
                a statement has a free variable), `rules` lists standing
                rules, `forget <rule-id>` revokes one; `runs` lists the rc
                lifecycle run log (create/fork/attach/drift/tick/rotate —
                the durable "did the rc lifecycle actually fire" checklist),
                `runs <run-id>` shows one run's per-script detail
mcp             list (alias status), reload — external MCP servers (mcp.toml: kaibo,
                bevy_brp, …); configured-vs-actually-running visibility + reconcile
midi            list, show — CRDT-owned MIDI device profiles at
                /etc/midi/devices/<name> (docs/midi-next.md)
model           Show a context's effective model (--context <ref>)
models          List configured providers, their models, and --model aliases
play            Play a sample now, or commit it as a clip cell onto a track with
                --track/--at/--label (docs/pcm.md)
policy          show, set — a registered instance's per-call QoS policy
preset          list, show, save, remove, reseed
rc              add, list, rm, show, edit, reset — lifecycle scripts (/etc/rc/<type>/<verb>/)
roster          status <text> [--availability], list — the live roster: post
                your own self-reported status (identity is always the
                caller's own) or list who's around right now
search          <pattern> — regex search across blocks (--all, --context, --kind, --role)
stage           commit, status, include, exclude — curate a staged (liminal) fork
transport       attach, detach, play, pause, stop, tempo <bpm>, ooda <on|off>,
                clock <system|modeled>, rotate, delete — a track's beat clock
                (the musician playhead)
vfs             snapshot <path> (--depth, --max-entries), activity [path] —
                recursive listing + generation stamps / per-directory heat totals
workspace (ws)  list, show, create, add, bind, remove
```

Run `kj <command> help` for detailed subcommand reference.
