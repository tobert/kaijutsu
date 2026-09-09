# The `kj` verb class: one declaration of effect per verb

**Delete this document when the last slice below ships.** It is a build
plan, not a reference. When every slice is done, the code and
`docs/gate-policy-tuning.md` carry everything here that is worth keeping;
melt the decision notes into `docs/devlog.md` and remove this file.

## The rule

Every `kj` verb declares its effect where the verb is declared, in code,
and that declaration is the only place the question is answered.

```rust
pub enum Effect { Read, Write, Destroy }

pub trait Classify { fn effect(&self) -> Effect; }

impl Classify for PresetCommand {
    fn effect(&self) -> Effect {
        match self {
            Self::List | Self::Show { .. } => Effect::Read,
            Self::Save { .. } | Self::Reseed => Effect::Write,
            Self::Remove { .. } => Effect::Destroy,
        }
    }
}
```

- `Read`: no side effect anywhere. Not the kernel database, not the host
  filesystem, not a remote, not a peer. A Read verb skips the shell gate
  and the classifier by construction.
- `Write`: changes state that can be changed back. Goes through the gate
  tiers like any other statement.
- `Destroy`: permanent, or takes something down that cannot be brought
  back. Latched: the dispatcher refuses without `--confirm`, always.

The match is over the parsed value, not the leaf name, so an argument that
changes the effect is visible to it: `block cat <id>` is Read and
`block cat <id> --out <path>` is Write, in one arm.

The match is exhaustive. A new variant with no arm is a compile error.
That is the whole coverage mechanism; there is no sidecar to keep in step.

## Why this replaces what was there

Before this doc, "does this verb write?" was answered in three places that
never checked each other: an authored `mutates` bit in
`contrib/kj-expectations.toml`, a two-token pass list in
`kj/readonly.rs`, and the handler body. The TOML also carried
`confirm_gated`, a fact about the handler that a coverage test could only
check for presence, never for truth. And it carried `expect`, the severity
the lfm2d scorer is expected to output, which is a test fixture for an
external classifier and not kaijutsu vocabulary at all.

Amy, 2026-09-09: *"the declaration whether something mutates should be
required on every kj verb, and can be accessed there directly as source of
truth. I think at one point I asked for a file export and it got
over-generalized."* And: *"I don't expect the classifier to ever learn kj
vocabulary and we control the code here. 'expectations' shouldn't surface
in kaijutsu really, it's an internal lfm2d concern that will change rapidly
as we learn how to make it work better and better."*

## The entry point

```rust
pub fn classify(argv: &[String]) -> Result<Effect, ClassifyError>
```

`argv` is the `kj` argv without the leading `kj`, exactly as the
dispatcher receives it and as the shell plan carries it. `classify` parses
the whole argv through a root `KjArgs` derive enum whose variants wrap the
42 domain `*Args` structs, and asks the parsed value for its effect. It
runs no handler and touches no kernel state.

`kj_command()` becomes `KjArgs::command()` plus the two root flags. The
reflected leaf set must not change: the corpus coverage test in
`kj/corpus.rs` guards that until slice 4 moves it.

A parse failure is a classify failure. The caller decides what that means;
for the shell gate it means "not read-only", which falls through to the
gate and the classifier. An unresolvable verb is never waved through.

## Slices

Each slice names the test that is red before it lands.

### Slice 1: Effect, Classify, the 42 impls, KjArgs, classify()

`kj/effect.rs` holds `Effect`, `Classify`, `KjArgs`, `classify`, and the
root delegation. Each domain module implements `Classify` for its own
`*Args` struct, delegating to its subcommand enum where it has one. A verb
with no subcommand (`cp`, `play`, `attach`, `fork`, `drive`, `wait`,
`diff`) implements it on the struct directly.

Seeding: the TOML's `mutates` and `confirm_gated` bits are the migration
source. `mutates = false` and present in `READ_ONLY_TABLE` is Read.
`confirm_gated = true` is Destroy. Everything else is Write. Where the TOML
says `mutates = false` but readonly.rs excludes the pair, the reason is an
argument-dependent write (`block cat --out`, `cas get --out`) and the arm
inspects the argument.

Red test: every leaf reflected from `kj_command()` classifies through its
synthesized clause. The mutation is a variant with no arm, which does not
compile.

### Slice 2: readonly.rs reads the class

Conditions 1 to 5 of `is_read_only_kj` stay, with their tests: name is
exactly `kj`, no redirect, no background, no heredoc, every argument plain.
Condition 6 becomes `classify(args) == Ok(Effect::Read)`.

Deleted: `READ_ONLY_TABLE`, `READ_ONLY_NO_SUBCOMMAND`, `MUTATING_TABLE`,
`every_kj_subcommand_is_classified`, and the module prose about the
two-token depth limit. `is_gate_exempt_kj` and `program_is_gate_exempt`
stay: the whole-verb `kj ledger` exemption is the gate's answer path, a
different rule from read-only, and it survives unchanged.

Red tests: `kj backend default show` is read-only. `kj block cat <id>` is
read-only and `kj block cat <id> --out x` is not. `kj transport list` is
read-only. All three are wrong today.

### Slice 3: confirm moves to dispatch

`KjDispatcher::dispatch` classifies before routing. `Destroy` without
`caller.confirmed` returns `KjResult::Latch` from that one place, with
`command` set to `kj <path>` and `target` to the first positional. The
eight handler-local `caller.confirmed` checks are deleted.

Decided (Amy, 2026-09-09): always latch Destroy. Two handlers lose
something: `character retire` only latched when live contexts existed,
and `doc delete` put the block count and a cascade warning in the message.
A generic message replaces both. If the richer text is missed, one optional
`describe` hook on the class is the way back, not a handler check.

Red test: dispatching every Destroy leaf with an unconfirmed caller returns
a Latch, and no handler runs. The mutation is deleting the dispatch check.

### Slice 4: the lfm2d fixture leaves the kernel

`kj/corpus.rs` moves into the `lfm2d-probe` example. Severity is derived
from Effect for kj verbs: Read is informative, Write is situation-normal,
Destroy is data-critical. The fixture file holds only per-clause overrides
where lfm2d calibration disagrees with that derivation, plus the extra
clause families (severity probes, data-position controls, benign
controls). It is named for the probe and lives beside it. The `Severity`
type leaves the kernel crate. `contrib/kj-expectations.toml` is deleted.

Red test: the probe's coverage test, now beside the probe, still fails on a
leaf with no clause and on an override naming no leaf.

### Slice 5: docs and the gate tier

`docs/gate-policy-tuning.md` "Generated tier" already says the builtin
tier is `Effect::Read`, and that a Write verb that should be allowed by
default (`kj handoff note`) is a rule in the layers above, not a field on
the class. Slice 5 confirms that section against the shipped code, retires
the issues.md pointers at the corpus, folds this doc's decision notes into
the devlog, and deletes this doc.

## Decisions carried here

- **Three rungs, not two bools.** "Does not mutate but needs confirmation"
  is not a state we want expressible.
- **The class answers effect, not permission.** A Write verb that policy
  should allow by default is a gate-policy rule, so the class stays honest
  about what the verb does. Amy can reverse this by putting an authored
  allow on the class; the doc notes the choice so the reversal is a
  decision, not a drift.
- **`kj transport list` becomes Read.** It was kept out of the read-only
  tables on Amy's instruction to keep the whole verb out of that module.
  With the class on the verb there is no module to keep it out of, and its
  own doc calls it read-only. Flagged for veto on the slice 2 landing.
- **`${VAR}` in a typed slot fails closed.** Plan-time argv carries
  `${VAR}` as literal text. A String positional parses. A numeric flag does
  not, so `kj wait ${CTX} --timeout ${T}` is not classified and meets the
  gate. Accepted: it is fail-safe, and a lenient parse would be a second
  parser to keep honest.
- **Eight confirm sites, not seven.** The TOML header said seven. The
  count that matters after slice 3 is one.
