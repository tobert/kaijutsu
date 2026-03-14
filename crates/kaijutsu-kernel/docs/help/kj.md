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
context (ctx)   list, info, switch, create, set, log, move, archive, remove, retag
fork            Fork current context (--shallow, --compact, --as)
drift           push, pull, merge, flush, queue, cancel, history
preset          list, show, save, remove
workspace (ws)  list, show, create, add, bind, remove
synth           all, <ctx>, status — semantic indexing + keyword synthesis
```

Run `kj <command> help` for detailed subcommand reference.
