# kj drift — cross-context communication

Drift transfers knowledge between contexts without sharing conversation history.

## When to Use What

- **push** — You have a specific fact to share. Fast, no LLM. Stages only; run `flush` to deliver.
- **pull** — You want a digest of another context's work. LLM reads their blocks and writes a summary into yours.
- **merge** — Your fork is done. LLM summarizes your work into the parent context.

## Staging

Pushes are staged, not delivered immediately. This lets you batch and review:

```bash
kj drift push impl "finding one"
kj drift push impl "finding two"
kj drift queue                     # see what's staged
kj drift cancel 2                  # changed your mind
kj drift flush                     # deliver remaining
```

## Subcommands

```
push <dst> <content>     Stage content for target context
pull <src> [prompt]      Pull + LLM-distill from source
merge [ctx]              Summarize this fork back into parent
flush                    Deliver all staged drifts
queue                    Show staging queue
cancel <id>              Remove staged drift by ID
history [ctx]            Show drift edges for a context
```

## Multi-Agent Pattern

```bash
# Context A finds something:
kj drift push B "the auth module uses JWT, tokens in Redis"
kj drift flush

# Context B wants A's full picture:
kj drift pull A "what did they find about error handling?"

# Context B (a fork of main) is done:
kj drift merge
```
