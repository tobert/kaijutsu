# Approval identity

Every character instance that does work answers to one other character instance: its **senpai**. The senpai's
character reviews the work. The character performing an operation cannot approve it, from any context, except the
self-confirmation at a root console, which nobody else may answer. The design and its decisions are in
`docs/senpai-kohai.md`; this file is the approval contract that follows from it. There is no configured default
reviewer, no per-context override, and no grant that moves review from one character to another.

```sh
kj character create coder
kj kohai create repair --type coder --as coder
kj senpai show repair
```

`kj kohai create` starts an instance of coder, with `repair` as its conversation, as a kohai of the caller's
instance. Run from Amy's root console, Amy's console is its senpai and Amy reviews coder. Run from banto's
conversation, banto's instance is its senpai and banto reviews coder. Creating a context does not grant review
authority to anyone beyond what the senpai edge gives.

## The senpai edge

`character_instances.senpai_instance` names the senpai instance. It is null only for a root console. A context
belongs to at most one instance (`contexts.instance_id`); a context with none (scratch, the handoff log, the drift
queue, lost+found, the beat score) routes nothing. The edge points at an instance, not at a context, so a rotation,
which starts a new conversation in the same instance, moves neither the senpai nor the kohai.

- The **character** of an instance answers for it, and never changes.
- The **kohai** of an instance are the live instances whose `senpai_instance` is that instance.
- The chain never has a cycle. Setting an edge that would close one is refused.

`kj senpai show [<ctx>]` prints the instance, its senpai (label and character), the lineage root, and who would
review an ask by its character. `kj kohai list [<ctx>]` lists the live kohai with character and type. Each takes
any context of an instance and uses that context's instance.

## Routing

The reviewer of an ask raised in context K by actor A:

1. Let I be K's instance. A context with no instance refuses, naming K.
2. Let S be `senpai_instance(I)`. If S is null (I is a root console): when A is I's live root character, the ask
   is a self-confirmation; otherwise resolution refuses and names A and K.
3. The reviewer is S's character when it is live and is not A.
4. When S's character is A, climb: I becomes S, and repeat from 2. A banto that forks itself stays in its own
   instance, so its fork answers to the banto's senpai.
5. A retired character refuses by name. It never substitutes another character.

One resolver does this. Every ask, the model-turn check, `kj senpai show` and `kj ledger escalate` use it.

```
coder lane ──senpai──► banto instance ──senpai──► amy (root console)
  ask by coder          character: banto         character: amy
  reviewer: banto       ask by banto             ask by amy
                        reviewer: amy            self-confirmation
```

**Self-confirmation.** At a root console the actor is the instance's own character. The ask is raised with the actor as its
reviewer, the actor alone may answer it, and the ledger records the answer as a self-confirmation. A root character
can never be cast, so self-confirmation is a human's act. A model turn refuses to start when its performer would
review itself.

**Escalation** runs the same climb past the instance that yielded the current reviewer, and refuses at an
instance with no senpai, naming it.

**Lineage root.** The character at the top of the senpai chain. It must be a live root character, or the lookup
refuses with the reason.

## Authority

One rule: **the character of an instance's senpai, or its lineage root**, may

- move it to another senpai (`kj senpai set <ctx> <senpai-ctx>`, each naming any context of its instance);
- escalate its asks;
- rotate it (its own performer may also rotate it, with no ask);
- archive it.

An rc lifecycle may also act on another context of its own instance, so the coder's archive rc archives its
judge shadows.

A refusal names the rule: "only <ctx>'s senpai (<character>) or its lineage root (<root>) may <act>". Also true:

- A root character never performs an instance other than its root console.
- `kj kohai create --as` needs the Operator capability and a live character caller, and raises no ask. The new
  instance's senpai is the caller's instance, or the `--senpai <ctx>` instance the caller has authority over.
- A model-played context holds no authority over itself, so it cannot widen its own egress list or move its own
  senpai (`docs/egress.md`).
- A senpai move refuses while any context of the instance has pending asks. Performer and reviewer must resolve
  to live, distinct characters.

Create sets the edge, and nothing else moves it but `kj senpai set`: `kj fork` joins the forking context's
instance, and `kj context rotate` starts a new conversation in the same instance, so neither touches the senpai or
the kohai. Every client creation names its performer with `--as`.

## Three identities

| Field | Meaning |
|---|---|
| `principal_id` | Authenticated requester; retained for capability and redemption scope |
| `actor_id` | Character performing this invocation; the character of the context's instance for kernel model work |
| `reviewer_id` | Effective reviewer snapshotted on the invocation or ask (a field of the ask, not a column of the context) |

The context row carries `instance_id`; its instance carries the senpai edge the
reviewer is resolved from. Neither records a reviewer of its own, so nothing is
copied from `created_by`.

The kernel resolves a model turn's identity before it starts. Tools,
read-only and writable shells, nested `kj` commands, and hook bodies preserve
its requester and performer. When a new ask is recorded, the gate resolves
the current reviewer from the context's senpai edge. Moving the edge
therefore affects new asks even from an already running turn.
Provider text, reasoning, and tool calls are authored
by the performer. User prompts keep their requester; kernel-produced tool
results keep kernel authorship.

`kj drive` runs the driven turn with the caller's performer as its
requester, and writes the `--prompt` seed as that performer. When banto,
performing a turn Amy requested, drives a lane, the lane's turn, its tool
calls, and its asks carry banto as `principal_id`. The lane's own performer
and reviewer still play and review that turn. The self-approval check
compares the reviewer with the performer, not the requester, so a driver
that is also the lane's reviewer still answers the lane's asks. As the
asks' requester, the driver may also cancel them, and a `session` rule
learned on one covers the lane and the driver as its requester.

An app, TUI, ACP client, or MCP bridge acts as the character bound to its
credential. Navigating into a coder context does not turn Amy into coder.
External lead models need their own character-bound credential; two processes
using the same credential are the same character. The `user_initiated` flag
controls presentation, never approval authority.

The Bevy app, the TUI, `kaijutsu-mcp`, and `kaijutsu-acp` all accept the same
`--key-fingerprint <fingerprint>` (select one SSH-agent key) and `--key-file
<path>` (select a private-key file) pair, resolved by the one shared
`kaijutsu_client::key_select::resolve_key_source` (`docs/character.md`, "The
bridge identity: a key per model character"). The selectors are mutually
exclusive and do not fall back to another key. The app's displayed identity
and draft ownership come from the authenticated connection's `whoami` result.
The app has dedicated ledger controls: an ask sheet
(`crates/kaijutsu-app/src/ui/ask_sheet.rs`) and a ledger ribbon
(`crates/kaijutsu-app/src/ui/ledger_ribbon.rs`), alongside shell `kj ledger`
commands.

New ACP sessions select a kernel model performer at launch:

```sh
kaijutsu-acp --character coder
```

`--parent <label>` names the context new sessions are created under; without
it, the kernel's only live root console is used. The named character
becomes the performer, the new instance is a kohai of the instance of the
context that creates it, and the reviewer follows the senpai edge.
`kaijutsu-tui` and `kaijutsu-mcp` take the same `--character` (MCP also reads
`KAIJUTSU_CHARACTER`); a root's connection needs it, since a root performs
only its root console. Without `--character`, use ACP session loading
to attach to an already configured context. The flag selects a character;
`--context-type` still selects the rc bundle.

## Answering and withdrawing asks

```sh
kj ledger show <request-id>
kj ledger allow <request-id>
kj ledger deny <request-id>
kj ledger escalate <request-id> --to amy
kj ledger cancel <request-id>
```

An ask snapshots its requester, performer, and reviewer. `show` reports all
three plus the deciding character. The assigned reviewer may allow or deny.
The assigned reviewer, the character of the ask context's senpai, or its lineage
root may pass a pending ask up the senpai chain with `escalate`. Naming the
next reviewer with `--to`, or taking the ask over, is the lineage root's
alone; this lets Amy reclaim an ask in her lineage when its reviewer is
unavailable. Escalation records both reviewer identities
and the actual caller, and refuses to assign the performer. The requester or
performer may cancel a pending ask. Cancellation runs nothing; it does not
undo an executed action.

Clients read and answer the ledger with typed RPCs that take no context:
`listAsks`, `getAsk`, `decideAsk`, and `escalateAsk`, with
`subscribeLedgerEvents` pushing every ask that changed after a generation.
The caller is the connection's principal. The ledger row is the record of an
answer: `decideAsk` authors no block in any transcript, and the asking
context learns the outcome through its own completion notice or held turn.
Amy, 2026-10-08: "the ledger is more about accounting than enforcement";
reviewer assignment is the boundary, and the ledger enforces it.

Only the assigned reviewer answers. To answer an ask assigned to someone
else, the lineage root first reassigns it to herself (`kj ledger escalate
<id> --to amy`, or `escalateAsk`), then answers. The ledger records each
reassignment with the reviewer before and after and the caller who moved it,
and `kj ledger show` lists them as `reassigned:` lines; a caller who names
itself took the ask over.

The TUI displays the authenticated character and offers approval controls only
to the assigned reviewer. Each card carries the ask's tool call, the lineage
root, and whether the ask has a program, and `AskSummary` supplies the rules
clients gate on: `can_escalate` (the pending ask's reviewer),
`can_take_over` (the lineage root, when it is neither reviewer nor performer),
and `can_remember_for_character` (a reviewer of an ask with a program).
In a client the root's way to reassign is a takeover, an escalation to itself;
the kernel also lets the root reassign to a third character through `kj ledger
escalate --to`. The card does not carry whether the context is archived, so
the kernel's refusal stands for that case. `kj ledger show` prints the lineage
root. A second context is unnecessary. The global ledger
also exposes asks raised in other contexts. ACP permission prompts follow the
same eligibility rule; the kernel remains authoritative if the assignment
changes while a prompt is open. Timeouts, cancelled ACP prompts, and transport
errors leave the ask pending; only an explicit decision records a verdict.

A senpai model reads the kohai's response and `kj ledger list|show` to
evaluate its asks. Reviewer assignment does not automatically schedule a
turn in one of that character's contexts.

Direct commands retain their connected actor. If that actor is also the
assigned reviewer, the ask is a self-confirmation: that actor answers it,
cancels it, or reassigns it to a different character. Human status is not
inferred from a client type.

## Standing rules for a character

```sh
kj ledger allow <request-id> --remember character --family
kj ledger rules
kj ledger forget <rule-id>
```

A `character` rule applies to the character that performed the ask, in
every context it performs in, and to no other character. Scope `session`
covers the asking context and requester; scope `always` covers everyone.

A character allow covers a `kj` call that acts on a context only when the
character holds the target's senpai. It covers these verbs:

| Verb | Target | Not covered |
|---|---|---|
| `kj drive [<target>]` | the target, default the current context | |
| `kj interrupt <target>` | the target | |
| `kj context archive [<context>]` | the context, default the current context | `--children` |
| `kj kohai create [--senpai <c>]` | `--senpai`; without it, the new context is the caller's own kohai | |

The gate resolves the target the way the verb does (label, id, `.`,
`.parent`). Every argument must be a literal word, because an expanded
argument could name another context or a flag, and a call with `--` is
not covered. A `kj` call that the builtin layer allows (a read, `kj
ledger`, `--help`) passes. Any other `kj` call is not covered, and
neither is a host command spelled `kj` or `kjc` (`./kj`, `kjc -c ...`),
which can reach a verb whose target the gate does not read. Nor is a
wrapper whose arguments name `kj` or `kjc` (`env kj ...`, `xargs kj`,
`bash -c 'kj drive x'`): its family key would cover the wrapper with any
target. A character allow is not learned from a program that contains
any of these; the answer names the command, and the decision on that ask
still stands. A family rule on a path (`source <path>`, `bash <path>`)
covers that path in later calls whatever the file then holds, so prefer
an exact rule for scripts. A
character deny applies without the target condition.

The gate checks the target when it decides, and the verbs do not check
it again. A senpai change that lands between the decision and the run
is not seen by that call.

For example, Amy answers banto's ask for `kj drive lane-1 --prompt ...`
with `--remember character --family`. Banto then drives any lane whose senpai it holds with no ask. It still
asks to drive a context whose senpai Amy holds, including its own conversation, and
the rule does not reach another character. An exact rule (`--remember
character` without `--family`) checks the target too, so the same statement
asks again after its target gets another senpai.

To be decided in step 2: `docs/senpai-kohai.md` does not say how the standing-rule target
condition reads once `director_id` is gone. This section assumes "the character holds the
target's senpai", the first half of the authority rule.

Commands other than `kj` carry no target that the gate reads. A
character family rule for them applies wherever the character performs,
by family key: `cd /tmp/x && git log` teaches `cd /tmp/x` and `git log`,
and `python3 -m unittest` teaches bare `python3`, because its first
argument is a flag. The answer names the keys it learned. Prefer an exact
rule when a family key is wider than intended.

The rule binds the character, not the context type: another character's
context of the same type is not covered. The reviewer who answers the ask makes the rule
and alone can forget it, and the rule applies even where another
character reviews this character's work. `kj ledger rules` shows the
scope as `character <name>`. Grants that need no first ask, through
character rc, are planned (`docs/character.md`, "Current
implementation").

## Persistence and replay

A caller's retry redeems an answer only when the requester, performer,
context, statements, and label match. The approval worker redeems an ask
with a stored command by its id.
Changing performers revokes that context's learned session rules. Broader
rules remain governed by their explicit scope. A linked model approval whose
performer changed is consumed without execution; restoring the old performer
does not revive it. Malformed or partial output-pair linkage refuses replay.
Linking an executable ask also retains the original pair's execution receipt;
model Waiting publication commits both together. Context and performer must
match, and an established pair or owner cannot be replaced. Receipt lookup
through the original ask survives later result-review asks.

Old asks with no recorded performer/reviewer remain audit history and cannot
be approved through a guessed identity. The senpai migration
(`docs/senpai-kohai.md`, "Migration") maps each live context to a senpai from how it
was routed, reports every mapping that was not the nearest-ancestor case, and
refuses to run rather than guess. It never derives a senpai from requester
identity. A missing performer still requires assignment before kernel model work.

## Continuation windows and async work

An ask has no default expiry. Its captured operation can await an explicit
decision without a human-time deadline. Cleanup may later cancel
obsolete requests with a recorded reason; elapsed waiting alone does not
invalidate the approval request.

A **continuation window** governs automatic model resumption after yielding.
It expresses expectations about retained KV state and the cost of continuing,
not a known provider-cache expiry. Ending the window does not expire the ask,
cancel its command, or erase its result. An explicit signoff can end a
continuation before the window would otherwise close. `/config/kernel/
continuation.toml` owns the policy: its shipped `[gate_resume] window_secs =
1800` keeps the window open for 30 minutes after the last actual provider
inference request. Each inference request, including a tool-loop iteration,
refreshes that time. Yielding, polling, and tool activity do not. `kj handoff
signoff <note>` closes the window immediately. `kj interrupt <target>` (and
the RPC `interruptContext`) records a separate, independent fact —
`interrupted_at`/`interrupted_by` on the epoch — that also closes the
window; unlike signoff, this is a senpai or reviewer stopping the loop
from outside, not the performer's own handoff close, and it does not set
`signed_off_at`. Stopping the running turn alone is not enough: every turn
end records a yield even when cancelled, so without this fact a later async
shell completion would still restart the chain.

Changing a context's performer closes its continuation and invalidates all
previous epochs in the same transaction as the assignment. Queued automatic
startup and each later inference attempt reject those epochs, including after
assigning the original performer again. An inference admitted before reassignment
may finish under its original performer; the next request is refused. Explicit
turn preparation rechecks the performer before opening an epoch.

Signoff, interrupt, and a newer explicit drive only close automatic
resumption. Already accepted turns can finish, but their requests do not
refresh a closed window or a newer epoch. Senpai changes preserve the
performer's continuation. Opening a new epoch (`kj drive`) clears a prior
interrupt along with the rest of the epoch's state.

Async shell submission returns a stable receipt naming the operation and any
approval dependency. Completion is a separate durable fact,
not a replacement for an acknowledgement already sent to the model. This
keeps earlier conversation content stable while the coder does independent
work, checkpoints, or signs off. The result must remain discoverable even
when no model is automatically resumed.

`kj wait` waits for its target context to have no accepted turns left in flight
and evidence that a turn ran. A terminal event cannot finish the wait while
another turn remains. Events supply the latest observed terminal detail;
quiet polling uses the block log and live turn registry if events are lost.
A terminated subscription switches to paced state polling. A pending ask in
the target context whose stored reviewer is the waiter ends the wait first,
with status `ask`, the ask's id and description, and the `kj ledger` commands
that answer it: the lane is blocked on the waiter.

Select a shell operation with `--operation <id>`, an ask with `--ask <id>`,
or a native kaish job with `--job <integer>` (optionally selecting a context).
Job IDs belong to the current process and context; durable operation IDs remain
readable after restart or job cleanup. A wait timeout ends that wait only;
it neither cancels work nor expires an ask. Waiting on an ask's decision and
waiting on the approved command's completion are distinct conditions. A kaish
job wait reports status and the captured command exit code. If durable
publication fails, the job can finish while its
operation remains pending; use `--operation` to inspect durable completion and
notification status. Publication failure does not change the command's exit.
A terminal-result write failure reports `state.retention_error` while the kernel
keeps the first result in memory and retries persistence. Result-review ledger
reads report `result_review.retention_error`. Recovery never reruns the command
or its result hooks. Shutdown reports results that remain non-durable.
Waiting must not hold a context
execution lock that prevents another invocation from writing a handoff or
observing completion. Preserve the rule that no RPC waits indefinitely.

The coder maintains a handoff while active and updates it before an
explicit wait or signoff. Record outstanding operation and ask IDs with the
objective, progress, evidence, and next actions. Beyond the continuation
window, default to leaving results for explicit drive or rotation instead of
starting a turn solely to recover a signoff from a cold conversation. A
successor inspects outstanding asks and completed results. It does not inherit
an old context's redemption authority. Re-asking and cancellation of a
superseded request need explicit linkage so rotation does not duplicate work.

## Current implementation

- A model's in-kernel turn holds on an ask its own tool call raised, and
  continues when the answer lands (`docs/gate-resume.md`, "The turn holds").
  The MCP and RPC shell paths return `Pending` immediately.
- Shell uses `run_in_background: false` by default and waits for the result.
  Both modes execute kaish and use the same gate. An unknown old `foreground`
  parameter is rejected. `run_in_background: true` returns a stable, non-error
  receipt with `operation_id` instead of waiting; a gate receipt may also
  carry `ask_id` and status `waiting`.
- The original model receipt stays `done`. The operation has a separate,
  excluded command/output pair, followed by a completion notification.
- A denied or cancelled model pair settles before notification. Its notification
  and answer redemption commit together; a failed write leaves the answer
  available. A session reads its pair directly and consumes the answer after
  successful settlement. Repeating a delivered refusal creates no second
  notification. Restart recovery for old answers remains limited as described
  in `docs/issues.md`.
- Native kaish jobs use a manager per context, so jobs survive materialized
  shell instances. The RPC shell path runs kaish in a detached task to preserve cwd, env,
  and session switching, while its job registration supplies the same waiting
  and cancellation handles.
- The continuation window is 30 minutes after the last actual provider
  request, including each tool-loop iteration. `kj handoff signoff <note>`
  closes it immediately. An approval still executes and records its result
  outside the window; automatic model resume requires the matching yielded,
  unsigned continuation epoch.
- Restart abandons unresolved asks and unfinished operations, and archive
  abandons that context's asks.
  Having no default expiry does not change those policies. Restart survival and
  rotation recovery need an explicit execution/recovery design before removal
  of those cleanup paths.
- Reviewer resolution is one function over `character_instances.senpai_instance` (the climb above,
  `KernelDb::route_review`). A retired senpai character refuses by name. `kj ledger escalate` without
  `--to` runs the same climb past the current reviewer and refuses at an instance with no senpai.
- Self-confirmation: an ask whose reviewer is its own actor is answerable only by that actor, and the
  decision is recorded as a self-confirmation. An instance with no senpai self-confirms only for the live
  root character whose console it is (`CharacterRow::root`) and refuses for anyone else. The performer-cannot-approve rule
  holds for every other ask.
- Character rules: `approval_rules` and `approval_rule_families` record the ask's `actor_id`, and scope
  `character` matches it. The kernel decides the target condition per command
  (`kj::gate_policy::serves_character`) and passes it to the ledger's match
  (`approval_ledger::rules::RuleMatch`); `kj ledger allow` refuses a character allow over a `kj` call
  the condition does not read (`gate_policy::character_rule_refusal`).
- Authority: the one rule above, checked at each act it governs. The lineage root is the character at the top
  of the senpai chain.
