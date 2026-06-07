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
# Fork two approaches from the current context
kj fork --name approach-a
kj fork --name approach-b

# Work in approach-a, then pull findings into parent
kj context switch .parent
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
block           list, ls, inspect, count, read, create, append, history, diff, edit, status
cache           list, add, clear — Claude prompt-cache breakpoints
cas             put, get, ls, info, rm — content-addressed blob storage
context (ctx)   list, info, switch, create, set, log, move, archive, remove, retag
doc             list, tree, create, delete — storage layer (all kinds, not just conversation)
fork            Fork current context (--shallow, --compact, --as)
drive           Clock one autonomous turn on a context (--prompt)
drift           push, pull, merge, flush, queue, cancel, history
transport       play, pause, stop, tempo <bpm>, ooda <on|off> — composer beat/playhead control
prompt          list, show, inject — MCP prompt discovery and injection
preset          list, show, save, remove
search          <pattern> — regex search across blocks (--all, --context, --kind, --role)
workspace (ws)  list, show, create, add, bind, remove
synth           all, <ctx>, status — semantic indexing + keyword synthesis
```

Run `kj <command> help` for detailed subcommand reference.
