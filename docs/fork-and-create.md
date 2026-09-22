# Fork continues, create begins

Status: chosen by Amy 2026-09-22 ("option 1 for fork seems reasonable");
shipped the same day.

Two verbs mint context rows, and each means one thing.

- **`kj fork` continues the same performance.** The child copies the
  parent's performer, cast, workspace, shell, and env, and a selection of
  its block log (`docs/fork-filters.md`). The forking actor directs the
  child. Fork never changes who plays or what type the seat is; that is
  what `--compact`, `--include`, `--exclude`, and `--preset` are for:
  rotation, KV reuse, a windowed continuation.
- **`kj context create` begins a new performance.** It takes `--type`, a
  character `--as`, `--cast`, `--cwd`, and `--env`, and carries no history.
  A lane gets its brief from the prompt it is driven with. This is how a
  director makes a coder: `kj context create <label> --type coder --as coder`.

`--as` therefore means one thing across `kj`: the character who performs.
The template-subtree fork that used the same flag is deleted; nothing in
`assets/`, `contrib/`, or `docs/` used it.

## Where a created context sits

A created context is a child of the caller's current context by default;
that is what makes it accountable to the seat that made it
(`docs/character.md`, "Roots and rotation"). `--parent <ctx>` places it
under another context. `--top` places it directly under the caller's
lineage root, the root context at the top of the caller's own tree, so a
context that should not hang off a busy seat still hangs off a root. Only a
root context has no parent, and only boot creates those
(`ensure_root_contexts`). A create with no current context and no `--parent`
refuses and names `--parent`.

`--parent` and `--top` conflict. `--top` walks `forked_from` from the
caller's context to the first parentless ancestor; `KernelDb::lineage_root`
already performs that walk to find the root character, so extend or sibling
it to return the context id rather than adding a second walk.

## Implementation

`kj/fork.rs` has three variants: full, filtered, and compact.
`ForkKind::Subtree` is gone from `kaijutsu-types`, and a stored `subtree`
row fails `fork_kind_from_sql` loudly like any unknown value. `kj context
create --top` resolves through `KernelDb::lineage_root_context`, the same
`forked_from` walk `lineage_root` uses. Tests: `context_create_*` in
`kj/context.rs` and `lineage_root_context_names_the_top_regardless_of_its_character`
in `kernel_db.rs`. The rc and ledger test fixtures create from a registered
console context (`console_caller`) because a create needs a parent.
