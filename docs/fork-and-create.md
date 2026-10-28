# Fork continues, context create begins

Status: chosen by Amy 2026-09-22 ("option 1 for fork seems reasonable");
shipped the same day. Reworked 2026-10-18 for the senpai edge
(`docs/senpai-kohai.md`): `kj context create` replaces `kj context create`.

Two verbs mint context rows, and each means one thing.

- **`kj fork` continues the same performance.** The child copies the
  parent's performer, cast, workspace, shell, and env, and a selection of
  its block log (`docs/fork-filters.md`). The forking context is the child's
  senpai. Fork never changes who plays or what type the seat is; that is
  what `--compact`, `--include`, `--exclude`, and `--preset` are for:
  rotation, KV reuse, a windowed continuation.
- **`kj context create` begins a new performance.** It takes `--type`, a
  character `--as`, `--cwd`, `--env`, `--parent` and `--senpai`, and
  carries no history. Without `--cwd` it starts in the current context's cwd,
  as fork does; a current cwd that no longer resolves refuses the create and
  names `--cwd`. A lane gets its brief from the prompt it is driven with. This
  is how a director makes a coder: `kj context create <label> --type coder --as coder`.

`--as` therefore means one thing across `kj`: the character who performs.

## Where a created context sits

Two links are set, and they mean different things.

- **The senpai** is who the new context answers to: its asks, reports and
  escalations go to the holder of the senpai context. By default it is the
  caller's current context, which is what makes a lane accountable to the seat
  that made it (`docs/character.md`, "Roots and rotation"). `--senpai <ctx>`
  names another context, when the caller holds that context or is its lineage
  root. A fork's senpai is the forking context; a rotation's successor keeps the
  predecessor's.
- **`forked_from`** is provenance: the context the row was forked or created
  from. `--parent <ctx>` places it under another context. It routes nothing.

A create with no current context and no `--parent` refuses and names `--parent`.
`kj context create` and `kj fork` never mint a senpai-less row. Root consoles come
from `ensure_root_contexts` at boot and from `kj character create --root`.
`kj context scratch`, the handoff log, the beat's score context, the drift queue,
lost+found, and the cold-start document bootstrap still insert rows with no
senpai of their own; those are listed in `docs/issues.md`, "Parentless contexts
outside the root consoles".

The old `--top` flag placed a created context under the caller's lineage root.
`docs/senpai-kohai.md` does not list it among `kj context create`'s flags. To be
decided in step 2: whether `--top` survives (as `--senpai` naming the lineage
root, or not at all).

## Implementation

`kj/fork.rs` has three variants: full, filtered, and compact.
`ForkKind::Subtree` is gone from `kaijutsu-types`, and a stored `subtree`
row fails `fork_kind_from_sql` loudly like any unknown value. The rc and
ledger test fixtures create from a registered console context
(`console_caller`) because a create needs a senpai.
