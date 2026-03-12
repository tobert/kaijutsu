# kj fork — fork the current context

Creates a new context from the current one. The new context gets its own conversation history.

## Fork Kinds

- **full** (default) — Deep copy of all blocks. Full history in the fork.
- **--shallow** — Only recent blocks (default: last 50). Fast, lighter.
- **--compact** — LLM-summarized. Starts with a distilled summary instead of full history.
- **--as <template>** — Subtree fork: copies tree shape from a template context.

## Examples

```bash
# Basic fork with a label
kj fork --name debug-auth

# Fork with a note injected as drift block
kj fork --name explore --prompt "investigate the auth timeout"

# Shallow fork (last 20 blocks only)
kj fork --shallow --depth 20 --name quick-check

# Fork from a preset template
kj fork --name review --preset code-review

# When done, merge findings back
kj drift merge
```
