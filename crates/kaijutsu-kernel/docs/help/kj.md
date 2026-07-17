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
# Stage a concrete finding (no LLM, fast)
kj drift push main "retry logic in client.rs:142 drops errors silently"
kj drift flush
```

### Complete a Fork

```bash
# Summarize this fork's work back to parent (LLM distillation)
kj drift merge
```

## Commands

```
attach          Attach to an existing context and run its rc attach lifecycle
                (distinct from `transport attach`, which attaches to a beat track)
audio           beats — offline audio analysis (beat/downbeat tracking via beat-this)
binding         show, allow, revoke, reset — a context's tool-capability allow-set
                (cap tokens incl. rc-write, config-write, drive, fork, drift, transport,
                operator, exec, admin, or <instance>[:<tool>], facade:<name>, *, facade:*)
block           list, inspect, count, read, cat, append, history, diff, status, create,
                edit (insert|delete|replace)
cache           list, add, clear — Claude prompt-cache breakpoints on the active context
cas             put, get, ls, info, rm — content-addressed blob storage
config          list, show, set, edit, reset — CRDT-owned config at /etc/config
                (models.toml, system.md, theme.toml, mcp.toml) + per-client at /etc/client
context (ctx)   list, info, current, switch, create, scratch, set, unset, log, move,
                rename, archive, conclude, promote, demote, pause, resume, remove,
                retag, hydrate
cp              Copy a file between VFS paths via the streaming pump (-r not implemented)
doc             list, tree, create, delete — storage layer (all kinds, not just conversation)
drift           push, pull, merge, flush, queue, cancel, history, edge rm
drive           Clock one autonomous turn on a context (--prompt)
editor          open, keys, state, save, quit, list — kernel-owned vi editor sessions
fork            Fork current context (--name, --prompt, --preset, --model,
                --include/--exclude ranges, --compact, --as, --stage, --switch)
model           Show a context's effective model (--context <ref>)
models          List configured providers, their models, and --model aliases
play            Play a sample now, or commit it as a clip cell onto a track with
                --track/--at/--label (docs/pcm.md)
policy          show, set — a registered instance's per-call QoS policy
preset          list, show, save, remove, reseed
rc              add, list, rm, show, edit, reset — lifecycle scripts (/etc/rc/<type>/<verb>/)
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
