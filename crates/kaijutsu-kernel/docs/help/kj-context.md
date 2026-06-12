# kj context — context management

## Subcommands

```
list [--tree]                List contexts (flat or tree view)   (alias: ls)
info [<ctx>]                 Show context details (default: current)
current                      Show the current context            (alias: show)
switch <ctx>                 Switch to a different context        (alias: sw)
create <label> [flags]       Create a new context                (alias: new)
scratch                      Get-or-create the "scratch" context (alias: self)
set <ctx> [flags]            Update context settings
unset [<ctx>] --env KEY      Remove an env var from a context
log [<ctx>]                  Show fork lineage
move <ctx> <new-parent>      Reparent a context                  (alias: mv)
archive <ctx>                Soft-delete (latched — needs --confirm)
remove <ctx>                 Hard-delete (latched)                (alias: rm)
retag <label> <ctx>          Move a label to a different context (latched)
hydrate [<ctx>] [flags]      Set/clear the hydration window policy (Operator-gated)
```

`<ctx>` accepts a label, a full/short context id, or `.` for the current context.

## create / set flags

`create` and `set` share the same configuration flags, so a context can be
born fully configured instead of needing a follow-up `set`:

- `--name`, `-n` — Label (create only; alternative to the positional `<label>`)
- `--parent`, `-p` — Parent context (create only; defaults to the current context)
- `--model`, `-m` — Model, format `provider/model` (e.g. `anthropic/claude-sonnet-4-6`).
  A bare `model` resolves the provider from the registry default (errors if none).
- `--system-prompt` — System prompt text
- `--consent` — `collaborative` or `autonomous`
- `--cwd` — Working directory
- `--env` — Set an env var, format `KEY=VALUE`
- `--type` — Context type (selects which rc lifecycle scripts run; default `default`)

## Examples

```bash
# See the full context tree
kj context list --tree

# Create a context, fully configured in one shot
kj context create review --model anthropic/claude-sonnet-4-6 --cwd /srv/app
kj context create --name explore --parent review --consent autonomous

# Update settings on an existing context
kj context set review --model google/gemini-2.5-pro
kj context set . --env RUST_LOG=debug
kj context set . --system-prompt "you are a careful reviewer"

# Check current context details / walk fork lineage
kj context info .
kj context log .

# Remove an env var
kj context unset review --env RUST_LOG
```

## Hydration window (cost guard)

A windowed context hydrates only `[0, marker] ∪ last-window` instead of its
whole history — the cost guard for long-running contexts driven at tempo
(composer players). The marker pins a durable prefix; the tail slides.

```bash
kj context hydrate --window 16                  # window this context, marker at tail
kj context hydrate bass --window 16 --mark <block-key>
kj context hydrate bass --clear                 # back to hydrate-everything
```

`--window 0` is rejected; `--mark` must name a block in the target context
(fail-loud). Operator-gated; rc lifecycle scripts run privileged (composer
`create` rc sets `--window 16`). Design: `docs/chameleon.md` "RC-driven
hydration marker"; the fork-side keep-set sharing this shape is
`docs/fork-filters.md`.

## Model vs fork

`set --model` re-points an existing context at a different model. To branch
the conversation onto a new model while keeping the original intact, use
`kj fork --model <provider/model>` instead — see `kj fork --help`.
