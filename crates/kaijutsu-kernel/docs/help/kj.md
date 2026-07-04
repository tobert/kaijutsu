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
binding         show, allow, revoke, reset — a context's tool-capability allow-set
block           list, ls, inspect, count, read, create, append, history, diff, edit, status
cache           list, add, clear — Claude prompt-cache breakpoints
cas             put, get, ls, info, rm — content-addressed blob storage
context (ctx)   list, info, switch, create, set, log, move, archive, remove, retag, hydrate
doc             list, tree, create, delete — storage layer (all kinds, not just conversation)
drift           push, pull, merge, flush, queue, cancel, history
drive           Clock one autonomous turn on a context (--prompt)
fork            Fork current context (--exclude, --shallow, --compact, --as, --switch)
model           Show a context's effective model (--context <ref>)
models          List configured providers, their models, and --model aliases
policy          show, set — a registered instance's per-call QoS policy
preset          list, show, save, remove
rc              add, list, rm, show, edit, reset — lifecycle scripts (/etc/rc/<type>/<verb>/)
search          <pattern> — regex search across blocks (--all, --context, --kind, --role)
stage           commit, status, include, exclude — curate a staged (liminal) fork
transport       play, pause, stop, tempo <bpm>, ooda <on|off> — musician beat/playhead control
workspace (ws)  list, show, create, add, bind, remove
```

Run `kj <command> help` for detailed subcommand reference.
