# kj context — context management

## Subcommands

```
list [--tree]            List contexts (flat or tree view)
info [<ctx>]             Show context details (default: current)
switch <ctx>             Switch to a different context
create <label>           Create a new empty context
set <ctx> [flags]        Update context settings
log [<ctx>]              Show fork lineage
move <ctx> <new-parent>  Reparent a context
archive <ctx>            Soft-delete (latched — needs --confirm)
remove <ctx>             Hard-delete (latched)
retag <label> <ctx>      Move a label to a different context (latched)
```

## Examples

```bash
# See the full context tree
kj context list --tree

# Create and configure a context
kj context create review
kj context set review --model anthropic:claude-sonnet-4-5-20250929
kj context set review --tools deny:shell

# Check current context details
kj context info .

# Walk fork lineage
kj context log .
```

## Model Assignment

```bash
# Set model on current context
kj context set . --model anthropic:claude-sonnet-4-5-20250929

# Set model on a named context
kj context set explore --model google:gemini-2.5-pro
```

Model is immutable on a context once set — fork to change it.

## Tool Filtering

```bash
kj context set . --tools all              # unrestricted
kj context set . --tools allow:glob,grep  # allowlist
kj context set . --tools deny:shell       # denylist
```
