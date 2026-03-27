# kj fork — fork the current context

Creates a new context from the current one. The new context gets its own conversation history.

## Fork Kinds

- **full** (default) — Deep copy of all blocks. Full history in the fork.
- **--shallow** — Only recent blocks (default: last 50). Fast, lighter.
- **--compact** — LLM-summarized. Starts with a distilled summary instead of full history.
- **--as <template>** — Subtree fork: copies tree shape from a template context.

## Options

- `--name`, `-n` — Label for the forked context
- `--model`, `-m` — Override model (format: `provider/model`). Inherits from parent if omitted.
- `--prompt` — Inject a note into the fork as a drift block
- `--mcp-prompt` — Inject an MCP prompt (format: `server/name`). Use with `--arg key=value`.
- `--arg`, `-a` — Argument for `--mcp-prompt` (repeatable, format: `key=value`)
- `--preset` — Apply a preset's settings after forking
- `--pwd` — Override the working directory on the fork
- `--depth` — Block limit for `--shallow` (default: 50)

## Examples

```bash
# Basic fork with a label
kj fork --name debug-auth

# Fork with a different model
kj fork --name fast-check --model anthropic/claude-haiku-4-5-20251001

# Fork with a note injected as drift block
kj fork --name explore --prompt "investigate the auth timeout"

# Shallow fork (last 20 blocks only)
kj fork --shallow --depth 20 --name quick-check

# Fork from a preset template
kj fork --name review --preset code-review

# Fork with an MCP prompt injected
kj fork --name analysis --mcp-prompt myserver/code_review --arg language=rust

# When done, merge findings back
kj drift merge
```
