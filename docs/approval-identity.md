# Approval identity

Amy reviews by default. Delegation is explicit. The character performing an
operation cannot approve it, from any context. The assigned reviewer may be
Amy, a directing model, or a separate adjudicator character.

```sh
kj character create coder
kj context create repair --type coder --as coder
kj context info repair
```

The caller's acting character becomes the context's director. Creating or
directing a context does not grant review authority. Without an explicit
assignment or delegation, Amy reviews coder whether Amy, Banto, or an external
lead created `repair`.

`/config/kernel/approval.toml` names the default reviewer, as a character name
or full principal ID. The shipped value is `amy`. The reviewer must resolve to
a live character; a missing or retired character is an error, never a reason
to substitute the caller.

A broken default does not replace a valid explicit or delegated reviewer.
It clears the cached default, so work that needs default review refuses
until configuration is repaired. `kj context info` keeps the context's
metadata visible and reports a reviewer error when its assignment cannot
resolve. Direct client commands remain available for inspection and repair;
the gate still refuses an ask with no valid reviewer.

Reviewer resolution follows this order:

1. Explicit reviewer override on the context.
2. Explicit delegation for its director character.
3. Configured default reviewer, Amy.

Amy can delegate review across a director's coder contexts:

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
pending asks. Settle or cancel those asks first. Amy can reclaim an ask from
an unavailable reviewer, settle it, then revoke the delegation.

For an explicit context assignment, Amy runs:

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

The connected character becomes its director; the reviewer follows the same
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
The assigned reviewer or Amy, as the default review authority, may explicitly
reassign a pending ask with `escalate`. This lets Amy reclaim a delegated ask
when its reviewer is unavailable. Escalation records both reviewer identities
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
reviewer, it cannot confirm its own ask: it can cancel it or explicitly
reassign it to a different character. Human status is not inferred from a
client type.

## Persistence and replay

Redemption matches the requester, performer, context, statement, and label.
Changing performers revokes that context's learned session rules. Broader
rules remain governed by their explicit scope. A linked model approval whose
performer changed is consumed without execution; restoring the old performer
does not revive it. Malformed or partial output-pair linkage refuses replay.

Old asks with no recorded performer/reviewer remain audit history and cannot
be approved through a guessed identity. The policy migration removes old
automatic reviewer assignments from contexts; it does not manufacture
director relationships or grants from requester identity. Existing contexts
therefore use the configured default unless explicitly reassigned. A missing
performer still requires assignment before kernel model work.

## Continuation windows and async work

Design direction; the continuation policy and unified wait below are not yet
implemented. An ask has no default expiry. Its captured operation can await
an explicit decision without a human-time deadline. Cleanup may later cancel
obsolete requests with a recorded reason; elapsed waiting alone does not
invalidate the approval request.

A **continuation window** governs automatic model resumption after yielding.
It expresses expectations about retained KV state and the cost of continuing,
not a known provider-cache expiry. Ending the window does not expire the ask,
cancel its command, or erase its result. An explicit signoff can end a
continuation before the window would otherwise close. The precise policy,
configuration owner, and signals used to open or refresh the window remain
to be designed; do not infer cache warmth from tool activity or polling.

Async shell submission should return a stable receipt naming the operation
and any approval dependency. Completion should be a separate durable fact,
not a replacement for an acknowledgement already sent to the model. This
keeps earlier conversation content stable while the coder does independent
work, checkpoints, or signs off. The result must remain discoverable even
when no model is automatically resumed.

One explicit wait operation should cover shell operations, asks, and directed
model work. Extend the existing `kj wait` contract rather than add a competing
wait mechanism. A wait timeout ends that wait only; it neither cancels work
nor expires an ask. Waiting on an ask's decision and waiting on the approved
command's completion are distinct conditions. Waiting must not hold a context
execution lock that prevents another invocation from writing a handoff or
observing completion. Preserve the rule that no RPC waits indefinitely.

The coder should maintain a handoff while active and update it before an
explicit wait or signoff. Record outstanding operation and ask IDs with the
objective, progress, evidence, and next actions. Beyond the continuation
window, default to leaving results for explicit drive or rotation instead of
starting a turn solely to recover a signoff from a cold conversation. A
successor inspects outstanding asks and completed results. It does not inherit
an old context's redemption authority. Re-asking and cancellation of a
superseded request need explicit linkage so rotation does not duplicate work.

Current implementation differs at these points:

- A pending gate returns immediately. The model loop receives its tool result
  and can continue, including writing a handoff; pending does not itself force
  the turn to stop.
- Approval execution fills the original waiting tool pair in place and evicts
  the cached mailbox. The gate-resume driver can request another model turn
  without a continuation-window check.
- `kj wait` waits for a context's turn, not arbitrary operations or asks.
- `shell_write` with `background: true` runs host shell source outside kaish
  and skips the foreground approval gate; it still requires `exec` authority.
  It cannot become the default async mode without preserving kaish execution,
  approval identity, captured inputs, and command semantics.
- Restart abandons unresolved asks, and archive abandons that context's asks.
  Having no default expiry does not change those policies. Restart survival and
  rotation recovery need an explicit execution/recovery design before removal
  of those cleanup paths.
