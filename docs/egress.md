# Egress

Which hosts a context's shell may reach over the network, who decides, and what
a refused request looks like. Today this covers `curl`, the only network tool a
context shell registers. `git` and any later builtin that contacts an outside
resource go through the same policy, so one place monitors, classifies, and
constrains them. See `docs/issues.md`, "Egress: what stays open".

## The rule

A context reaches a host only when that context's egress list names it. A
context with an empty list reaches nothing: `curl` stays registered, refuses
the request before any connection opens, and its error names the host and says
the context's egress list does not allow it.

The list belongs to the context, not to its type and not to a file. It is rows
in `kernel.db`:

```
context_egress(context_id BLOB, host TEXT, PRIMARY KEY (context_id, host))
```

`host` takes one of three forms:

| Form | Meaning |
|---|---|
| a DNS name, `crates.io` | that exact name; no subdomain matching |
| a loopback literal, `localhost`, `127.0.0.1`, `::1` | every loopback address |
| `*` | every host, loopback included |

A DNS name that resolves to a loopback or link-local address is refused unless
the list also holds a loopback literal (or `*`). One loopback literal opens
all of loopback; the list cannot grant `127.0.0.1` and withhold `::1`. `kaish-tools-curl` checks each
resolved address before it connects, so a name cannot stand in for an address
the list did not grant.

Loopback is not granted by default. On a host like zorak it reaches the
kernel's own SSH port and local model servers, so a context gets it only when
its list says so. Tests that stand up a local mock server add `127.0.0.1` to
the test context's list.

## Hooks reach only their context's list

A pre_call hook body runs in a snapshot of the calling context's shell, so
its `curl` reaches exactly the hosts that context's list names. No host opens
for every context. An rc hook that needs a network service must have that
host on the list of each context it gates, or fail closed without it.

## Who changes the list

`kj context set <context> --egress-allow <host>` adds a row and
`--egress-deny <host>` removes one. Both repeat. The caller must hold the same
authority a performer assignment needs: the target's lineage root, its
effective reviewer, or its director (`kj/context.rs`, `context_set` and
`caller_may_assign_performer`). A model-played context is none of those for
itself, so it cannot widen its own list. `kj context info` shows the list.

An rc create script's `kj` acts as the context's creator, so a script may
set the list when the creator holds that authority. The benchmark adapter
opens egress this way (`docs/benchmarks.md`, `egress_allow`); kaijutsu ships no
such script.

`kj fork` copies the parent's rows to the child, so a child starts with what
its parent had and no more. `kj context create` starts with an empty list.

The shell reads the rows when it is built, and a context shell is built for
each use, so a change applies to the next command. A running `curl` keeps the
list it started with.

## What a miss does

A host that is not on the list is refused. Nothing asks a human and nothing
scores the request. The choices today are the list or `*`. Sending a miss to a
classifier or to the approval ledger is open work: the gate's plan evaluator
already sees `curl <url>` as a command, but an approval has no way to reach the
tool at connect time.
