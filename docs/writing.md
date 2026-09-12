# Writing and terms


Kaijutsu keeps a small, predictable instrument so existing skill transfers. This
guide keeps a small, predictable subset of English for the same reason. Read it
before editing prose, comments, published help, or documentation.

Absorbed from kaish's `AGENTS.md` (2026-08-18) and adapted — the rules are
shared, the vocabulary is ours. Where a rule collided with kaijutsu's existing
prose, the collision is named below rather than silently resolved.

## Vocabulary choices

Keep the vocabulary small. This limits the number of distinct words, not the
length of the text — a familiar word may need a longer sentence.

Use plain words instead of figures of speech. Make the intended meaning
available from the words themselves, including in second-language or
partial-context use.

Use an established technical term when kaijutsu gives it one meaning. Those
terms live in the Terms table below.

Use the public word instead of a tool's private term. Write "18% fewer
allocations," not "18% fewer blocks," because `block` already names a kaijutsu
concept — a private term that collides with a public one is the worst case.

Use American spelling to match the corpus: `modeled`, not `modelled`.

## One term, one meaning

Pick one word for each concept and keep it. Do not vary a word for style.

Two words carry kaijutsu meanings that differ from kaish's guide. Ours win in
this repo:

- **`surface`** is a real kaijutsu noun, not a hedge: a place a player acts on
  the instrument (the conversation surface, a write surface, the RPC surface,
  the `kj` surface). It is legitimate when the thing named is a surface. It is
  a hedge when it stands in for a specific artifact — then name the tool schema,
  the error message, the help topic, or the RPC method instead.
- **`seam`** is where two mechanisms are joined and one can be replaced. It is
  ours and it stays. Use `boundary` for the line that separates available
  actions from mechanism behind them. They are different words for different
  things; do not treat them as synonyms.

Cross-references take one form per target: `` see `kj help <topic>` `` for a
help topic, and `docs/<file>.md`, "Section name" for a design document. Link
instead of re-explaining.

## Provide specific values

Whenever it is practical, give the public exit code, size, flag, default, and
condition. This saves a round trip and gives a model a clear observation to
update against.

> Before: Oversize output fails.
>
> After: Oversize output spills to a file and exits 3.

State the default and the condition too — "reads stdin when no files are given",
"off by default; applies to `-r` only".

## Fast and informative failures

Make errors, warnings, and failures informative and, where possible,
instructive. Lead with the consequence, name the condition, and suggest the next
step when it is known.

**Code comments are technical, not historical.** No dates, rulings, quotes,
slice numbers, or commit hashes in a Rust comment or a schema comment — those
live in `git log`, `docs/devlog.md`, and `docs/issues.md`. State the rule the
code follows; a `docs/<file>.md` pointer is the exit to the why.

Text that faces users, agents, and models must not leak internals. An internal
module path is unresolvable to its reader and should appear only in an assertion
or an error that indicates a real defect in kaijutsu.

## Published text

Some of what we write is shipped to models as part of their working context.
Treat it as a product surface, not as a comment.

Published in kaijutsu:

- **`kj` help.** Every `///` on a clap struct field is reflected out of
  `kj_command()` into help that agents read. Describe the argument's behavior
  there. Put mechanism and rationale in `//` comments.
- **MCP tool schemas.** Tool and parameter descriptions reach every connected
  client.
- **`docs/kj-help/`.** Help topics, read by models mid-task.
- **rc scripts** under `/config/rc` — a `.md` block lands in the system-prompt
  slot. This is the most expensive prose in the repo; every token competes.
- **Error blocks.** `BlockKind::Error` text is read by the model that caused it.

> Before: `/// List rc lifecycle runs (the durable "did the rc lifecycle`
> `/// actually fire" checklist — approval_ledger::rc_runs), or, with a run`
> `/// id, show one run's per-script detail. A bare positional (not a --run`
> `/// flag) to match show <request_id>'s shape ...`
>
> After: `/// List rc lifecycle runs, most recent first. With a run id, show`
> `/// that run's per-script detail.`

Do not infer the published text by grepping the source — read what the reflection
actually emits, by running the command's `--help` or reading the tool schema.
When you touch a `kj` verb, audit every `///` on its clap struct; the visit
supplies the context needed to judge each line.

## Write for model context

Use the same prose in human and model contexts. Assume the context may be
truncated: put the rule before its justification, so a cut keeps the rule.
Teach syntax with examples. Repeat a rule in the error that enforces it.

## The example is the rule

Show the correct example before explaining it. Make the example carry the rule
by itself, so the explanation is confirmation rather than the only source.

`kj stage exclude <id> && kj fork` — exclude unwanted conversation history,
then hydrate a child without it. Stored system instruction exclusions affect
the next turn directly; see `docs/prompts.md`.

Avoid incorrect examples. When one is necessary, put the correct form first and
mark the error next to it:
`kj block read 'abc12345#7'`; `kj block read abc12345#7 # error — kaish eats the #`.

## Terms

Terms that carry a guarantee. **This table is the source.** The list grows when
a collision appears in real prose, not in advance.

| Term | Part of speech | Meaning |
|---|---|---|
| kernel | noun | The kaijutsu kernel. Not the OS kernel, and not kaish's execution core — say "kaish kernel" when you mean that one. |
| context | noun | The durable, kernel-sequenced, multi-writer side: block log, exclusions, edits, metadata. |
| conversation | noun | The live append-only message sequence shipped to the LLM, hydrated from a context at a boundary event. |
| document | noun | The storage primitive in the block store. A context is metadata on a `Conversation` document; the two words are not interchangeable. |
| block | noun | One authored unit in a document. Never use it for an allocation, a disk block, or a UI rectangle. |
| player | noun | Anyone acting on the instrument — human, model, connected app, sibling context. All players are inside one trust boundary. |
| capability | noun | An ergonomic nudge that narrows focus and removes a footgun. Never a security control, and never a statement that a player is less trusted. |
| loadout | noun | The set of capabilities a context is given. Where mistake-prevention is routed. |
| gate | noun, verb | The check that stops a statement to ask its assigned reviewer. The one place that authority lives. |
| ask | noun | A durable row a gate leaves behind, waiting for a decision. Answered through `kj ledger`. |
| continuation window | noun | Policy window for automatically resuming a model conversation after it yields. It lasts 30 minutes from the last actual provider inference request, not a yield. It expresses KV-cache and cost expectations, not a guaranteed cache lifetime or an ask expiry; see `docs/approval-identity.md`, "Continuation windows and async work". |
| fork | noun, verb | Making a child context. The structural parent edge; the context graph is a forest. |
| drift | noun, verb | An overlay edge between contexts, deliberately cyclic. Never unify it with fork. |
| hydrate | verb | To build a conversation from a context. Happens only at a boundary event. |
| sequence | verb, noun | What the kernel does to every accepted mutation. There is exactly one sequencer. |
| fail loudly | verb phrase | An error is explicit and immediate. We never continue on a wrong assumption, and we prefer crashing to corrupting. |
| character | noun | The persistent someone a name resolves to; human or model. A principal with a sheet (`docs/character.md`). For a text unit say code point, glyph, or `char`, never character. |
| context_type | noun | The rc bundle a context runs, `/config/rc/<type>/<verb>/`. A role, not an individual. Character rc composition is planned; see `docs/character.md`, "Current implementation". |
