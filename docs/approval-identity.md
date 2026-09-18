# Approval identity

The character responsible for the context above the work reviews it.
Delegation is explicit. The character performing an operation cannot approve
it, from any context — except the self-confirmation whose assigned reviewer
IS that character, which nobody else may answer. The assigned reviewer may be
a root character such as Amy, a directing model, or a separate adjudicator
character. There is no configured default reviewer.

```sh
kj character create coder
kj context create repair --type coder --as coder
kj context info repair
```

The caller's acting character becomes the context's director. Creating or
directing a context does not grant review authority. Run from Amy's root
context, `repair` is a child of it, so without an explicit assignment or
delegation Amy reviews coder. Run from banto's seat, banto reviews coder.

Every reviewer must resolve to a live character; a missing or retired
character is an error, never a reason to substitute the caller.
`kj context info` keeps the context's metadata visible and reports a reviewer
error when its assignment cannot resolve. Direct client commands remain
available for inspection and repair; the gate refuses an ask with no valid
reviewer.

Reviewer resolution takes the acting character (the actor) and the
context, and follows this order:

1. Explicit reviewer override on the context.
2. Explicit delegation for its director character.
3. The responsible character of the nearest context at or above the ask's
   own, walking `forked_from` upward, that is live and is not the actor. A
   context's responsible character is its performer, or its director when
   no performer is set. Starting at the ask's own context is how a context
   with no parent still resolves through its own director; a model turn's
   performer is that context's own responsible character, so the walk
   immediately climbs past it. Archived ancestors are walked through, a
   retired responsible character refuses by name, and a `forked_from`
   pointing at no row is an error (`docs/character.md`, "Roots and
   rotation").

When every layer is exhausted and the actor is a live root character, the
actor is at its own root: the ask is raised with the actor as its reviewer,
the actor alone may answer it, and the ledger records the answer as a
self-confirmation. When the actor is not a root character, resolution
refuses and names the actor and the context; assign a reviewer with
`kj context set <context> --reviewer <character>`. A model turn refuses
to start when its performer resolves as its own reviewer, and a root can
never be a performer, so self-confirmation is a human's act. An explicit
override or delegation that names the actor resolves the same way, as a
self-confirmation, since the human set it up. Escalation runs the same walk
past the context that yielded the current reviewer, and refuses at a root,
naming it.

Any live root character can delegate review across a director's coder
contexts:

```sh
kj ledger delegation grant banto --to banto
kj ledger delegation list
kj ledger delegation revoke banto
```

The target can instead be a distinct adjudicator character. Delegation does
not create a model, select its inference provider, or start a review context.
It applies across that director's contexts without overriding explicit
context assignments. A director cannot grant this authority to itself.
Changing or revoking a delegation refuses while affected contexts have
pending asks. Settle or cancel those asks first. The lineage root of an ask's
context can reclaim it from an unavailable reviewer, settle it, then revoke
the delegation.

A context's **lineage root** is the character responsible for the context at
the top of its `forked_from` chain. It must be a live root character; a
lineage that ends anywhere else refuses. The lineage root changes approval
routing on its contexts. For an explicit context assignment, Amy runs, from
a context in her lineage:

```sh
kj context set repair --as coder --reviewer banto
```

`--clear-reviewer` removes that override and restores policy resolution.
`--director <character>` changes the director relationship; it does not itself
grant review authority. Assignment changes refuse while the context has
pending asks. Both performer and effective reviewer must resolve to live,
distinct characters. Ordinary forks preserve the director and explicit
override. Regular client creation leaves the performer unset, so model work
still needs an explicit performing character.

`kj context set <ctx> --as <character>` is allowed for a caller who is not
the target's resolved reviewer when the caller's actor directs the target,
which a fork or a create from the caller's own context records as
`director_id` (guidance, Amy 2026-09-15, "yes banto can cast its own
children"). Casting is what makes the cast character accountable to the
caller in that context; no sheet relation is consulted, and a root
character cannot be cast, by any caller, through `create --as` or
`set --as`. `kj context create --as` is the same act at birth: it needs the
Operator capability and a live character caller, who becomes the new
context's director. It raises no ask; a caller without that authority is
refused. This is a second way to earn `--as` authority,
not a change to reviewer authority: it does not let a director assign
`--reviewer` or `--director`, which still require the lineage root,
and the performer still cannot review its own work. The lineage root may
also assign `--as` on any context in its lineage.

## Three identities

| Field | Meaning |
|---|---|
| `principal_id` | Authenticated requester; retained for capability and redemption scope |
| `actor_id` | Character performing this invocation; the context's `played_by` for kernel model work |
| `reviewer_id` | Effective reviewer snapshotted on the invocation or ask |

The context row's nullable `reviewer_id` is the explicit override; an unset
override means resolve policy, not copy `created_by`. Its `director_id`
records the directing character independently of requester and performer.

The kernel resolves a model turn's identity before it starts. Tools,
read-only and writable shells, nested `kj` commands, and hook bodies preserve
its requester and performer. When a new ask is recorded, the gate resolves
the current reviewer under the same lock that serializes delegation changes.
Revoking a grant therefore affects new asks even from an already running turn.
Provider text, reasoning, and tool calls are authored
by the performer. User prompts keep their requester; kernel-produced tool
results keep kernel authorship.

An app, TUI, ACP client, or MCP bridge acts as the character bound to its
credential. Navigating into a coder context does not turn Amy into coder.
External lead models need their own character-bound credential; two processes
using the same credential are the same character. The `user_initiated` flag
controls presentation, never approval authority.

The Bevy app accepts `--key-fingerprint <fingerprint>` to select one SSH-agent
key, or `--key-file <path>` to select a private-key file. The selectors are
mutually exclusive and do not fall back to another key. Its displayed identity
and draft ownership come from the authenticated connection's `whoami` result.
The app currently reviews asks through shell `kj ledger` commands; it has no
dedicated ledger controls yet.

New ACP sessions select a kernel model performer at launch:

```sh
kaijutsu-acp --character coder
```

`--parent <label>` names the context new sessions are created under; without
it, the kernel's only live root context is used. The connected character
becomes its director; the reviewer follows the same
policy as other creation paths. Without `--character`, use ACP session loading
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
The assigned reviewer or the lineage root of the ask's context may
explicitly reassign a pending ask with `escalate`. This lets Amy reclaim a
delegated ask in her lineage when its reviewer is unavailable. Escalation records both reviewer identities
and the actual caller, and refuses to assign the performer. The requester or
performer may cancel a pending ask. Cancellation runs nothing; it does not
undo an executed action.

The TUI displays the authenticated character and offers approval controls only
to the assigned reviewer. A second context is unnecessary. The global ledger
also exposes asks raised in other contexts. ACP permission prompts follow the
same eligibility rule; the kernel remains authoritative if the assignment
changes while a prompt is open. Timeouts, cancelled ACP prompts, and transport
errors leave the ask pending; only an explicit decision records a verdict.

A directing model reads the coder's response and `kj ledger list|show` to
evaluate its asks. Reviewer assignment does not automatically schedule a
turn in one of that character's contexts.

Direct commands retain their connected actor. If that actor is also the
assigned reviewer, the ask is a self-confirmation: that actor answers it,
cancels it, or reassigns it to a different character. Human status is not
inferred from a client type.

## Persistence and replay

Redemption matches the requester, performer, context, statement, and label.
Changing performers revokes that context's learned session rules. Broader
rules remain governed by their explicit scope. A linked model approval whose
performer changed is consumed without execution; restoring the old performer
does not revive it. Malformed or partial output-pair linkage refuses replay.
Linking an executable ask also retains the original pair's execution receipt;
model Waiting publication commits both together. Context and performer must
match, and an established pair or owner cannot be replaced. Receipt lookup
through the original ask survives later result-review asks.

Old asks with no recorded performer/reviewer remain audit history and cannot
be approved through a guessed identity. The policy migration removes old
automatic reviewer assignments from contexts; it does not manufacture
director relationships or grants from requester identity. Existing contexts
therefore resolve through the walk unless explicitly reassigned. A missing
performer still requires assignment before kernel model work.

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
signoff <note>` closes the window immediately.

Changing a context's performer closes its continuation and invalidates all
previous epochs in the same transaction as the assignment. Queued automatic
startup and each later inference attempt reject those epochs, including after
assigning the original performer again. An inference admitted before reassignment
may finish under its original performer; the next request is refused. Explicit
turn preparation rechecks the performer before opening an epoch.

Signoff and a newer explicit drive only close automatic resumption. Already
accepted turns can finish, but their requests do not refresh a closed window or
a newer epoch. Reviewer-only changes preserve the performer's continuation.

Async shell submission returns a stable receipt naming the operation and any
approval dependency. Completion is a separate durable fact,
not a replacement for an acknowledgement already sent to the model. This
keeps earlier conversation content stable while the coder does independent
work, checkpoints, or signs off. The result must remain discoverable even
when no model is automatically resumed.

`kj wait` covers a context's model turn by default, or a shell operation with
`--operation <id>`, an ask with `--ask <id>`, and a native kaish job with
`--job <integer>` (optionally selecting a context). A wait timeout ends that
wait only; it neither cancels work
nor expires an ask. Waiting on an ask's decision and waiting on the approved
command's completion are distinct conditions. A kaish job reports the captured
command result. If durable publication fails, the job can finish while its
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

- A pending gate returns immediately. The model loop receives its tool result
  and can continue, including writing a handoff; pending does not itself force
  the turn to stop.
- Shell uses `foreground: false` by default. Both modes execute kaish and use
  the same gate. An unknown old `background` parameter is rejected.
  `foreground: true` waits for the result. The default returns a stable,
  non-error receipt with `operation_id`; a gate receipt may also carry `ask_id`
  and status `waiting`.
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
- Reviewer resolution takes the actor and the context and walks
  `forked_from` (`KernelDb::responsible_character_above`; guidance from
  Amy, 2026-09-15, and `docs/character.md`, "Roots and rotation").
  Explicit override and delegation resolve as configured even when they
  name the actor; the walk itself never returns the actor. A retired
  responsible character on an ancestor refuses, naming it, rather than
  being skipped. `kj ledger escalate` without `--to` runs the same walk
  excluding the current reviewer and refuses at a root.
- Self-confirmation: an ask whose reviewer is its own actor is answerable
  only by that actor, and the decision is recorded as a self-confirmation.
  An exhausted walk self-confirms only for a live root character
  (`CharacterRow::root`) and refuses for anyone else. The
  performer-cannot-approve rule holds for every other ask.
- Authority: `KernelDb::lineage_root` finds the root character at the top
  of a context's `forked_from` chain. It gates `kj context set --reviewer`,
  `--director`, and `--clear-reviewer`, and `kj ledger escalate` by someone
  other than the assigned reviewer. `kj ledger delegation grant|revoke`
  requires any live root character.
