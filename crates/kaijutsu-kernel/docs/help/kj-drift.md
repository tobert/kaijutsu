# kj drift — cross-context communication

Drift transfers knowledge between contexts without sharing conversation history.

## When to Use What

- **push** — You have a specific fact to share. Fast, no LLM. Delivered immediately.
  - Use `--summarize` (`-s`) to LLM-distill your whole context instead of sending literal content.
  - Use `--stage` to batch instead of delivering now (see Staging).
- **pull** — You want a digest of another context's work. LLM reads their blocks and writes a summary into yours.
- **merge** — Your fork is done. LLM summarizes your work into the parent context.

## Delivery

`push` delivers on the spot — the content is a durable block in the target's
document when the command returns:

```bash
kj drift push impl "the retry path drops errors silently"
# drifted → impl
```

If delivery fails, the content is **staged rather than lost** and the command
reports an error saying so; `kj drift flush` retries it.

## Staging

Pass `--stage` to queue instead of delivering. This lets you batch and review:

```bash
kj drift push --stage impl "finding one"
kj drift push --stage impl "finding two"
kj drift queue                     # see what's staged
kj drift cancel 2                  # changed your mind
kj drift flush                     # deliver remaining
```

## Subcommands

```
push <dst> <content>     Send content to target context (delivers now)
push <dst> --summarize   Send LLM-distilled summary of your context
push <dst> --stage ...   Queue for a later flush instead of delivering
pull <src> [prompt]      Pull + LLM-distill from source
merge [ctx]              Summarize this fork back into parent
flush                    Deliver all staged drifts
queue                    Show staging queue (yields queue u64 ids)
cancel <queue_id>        Remove staged drift before flush (pre-flush only)
history [ctx]            Show drift edges for a context (yields edge UUIDs)
edge rm <uuid>           Remove a post-flush drift edge by UUID
```

Two id namespaces:
- `cancel` takes the **u64 queue id** shown by `queue` (ephemeral, gone
  after flush).
- `edge rm` takes the **UUID** shown by `history` (persistent edge in
  the context DAG; pairs with the iteration handles `history` emits).

## Multi-Agent Pattern

```bash
# Context A finds something:
kj drift push B "the auth module uses JWT, tokens in Redis"

# Context B wants A's full picture:
kj drift pull A "what did they find about error handling?"

# Context B (a fork of main) is done:
kj drift merge
```
