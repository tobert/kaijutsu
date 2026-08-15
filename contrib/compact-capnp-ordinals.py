#!/usr/bin/env python3
"""Compact one Cap'n Proto interface's method ordinals, closing retired holes.

Cap'n Proto requires interface method ordinals to be sequential with no holes,
which is why retiring a method leaves a `retiredNN @NN ()` stub. A flag day is
the one moment those stubs can go, because renumbering shifts every later
ordinal — safe only when every binary is rebuilt together.

TRAP THIS TOOL EXISTS TO AVOID: ordinals are NOT in file order. `kaijutsu.capnp`
declares `getContextVersion @110` up beside the other block queries, hundreds of
lines above `retired37 @37`, because a new method is written where it belongs to
a reader and given the next free number. So renumbering by file position would
rewrite ~77 ordinals that did not need to change and silently reassign methods.
This works in ORDINAL order and preserves every surviving method's relative
position in the numbering, so the only ordinals that move are the ones after a
deleted hole — which is the minimum possible churn.

Struct field ordinals live in their own namespace and changing them would change
wire layout, so they are never touched.

Usage:
    compact_ordinals.py <file.capnp> <InterfaceName> [--check] [--drop-retired]

--check          report and exit 1 if anything would change
--drop-retired   also delete `retiredNN @NN ();` stub lines
"""

import re
import sys


def find_interface_body(text: str, name: str) -> tuple[int, int]:
    """Return (start, end) offsets of the interface's body, braces excluded."""
    m = re.search(rf"^interface {re.escape(name)}\b[^{{]*\{{", text, re.MULTILINE)
    if not m:
        raise SystemExit(f"interface {name} not found")
    start = m.end()
    depth = 1
    i = start
    while i < len(text):
        c = text[i]
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                return start, i
        elif c == "#":  # comment to end of line; braces inside are not syntax
            nl = text.find("\n", i)
            i = len(text) if nl == -1 else nl
            continue
        i += 1
    raise SystemExit(f"interface {name} is not closed")


# A method declaration: indentation, an identifier, then @N. Anchored to a line
# start so an `@N` inside a comment or a nested group is never matched.
METHOD = re.compile(r"^(\s+)([A-Za-z_][A-Za-z0-9_]*) @(\d+)\b", re.MULTILINE)
RETIRED_LINE = re.compile(r"^\s*retired\d+ @\d+ \(\);[ \t]*\n", re.MULTILINE)


def compact(text: str, name: str, drop_retired: bool):
    start, end = find_interface_body(text, name)
    body = text[start:end]

    if drop_retired:
        body = RETIRED_LINE.sub("", body)

    # (ordinal, ident) in ORDINAL order — the numbering's own order, which is
    # not the file's.
    found = [(int(m.group(3)), m.group(2)) for m in METHOD.finditer(body)]
    found.sort()

    remap = {old: new for new, (old, _) in enumerate(found)}
    changes = [
        (ident, old, remap[old]) for old, ident in found if remap[old] != old
    ]

    # Rewrite in one pass so a method that takes another's old number cannot
    # clobber it (a sequential search-and-replace would).
    def sub(m: re.Match) -> str:
        indent, ident, old = m.group(1), m.group(2), int(m.group(3))
        return f"{indent}{ident} @{remap[old]}"

    new_body = METHOD.sub(sub, body)
    return text[:start] + new_body + text[end:], changes


def main() -> int:
    if len(sys.argv) < 3:
        raise SystemExit(__doc__)
    path, iface = sys.argv[1], sys.argv[2]
    check = "--check" in sys.argv
    drop = "--drop-retired" in sys.argv

    text = open(path).read()
    out, changes = compact(text, iface, drop)

    for ident, old, new in changes:
        print(f"  {ident}: @{old} -> @{new}")
    print(f"{iface}: {len(changes)} ordinal(s) change")

    if check:
        return 1 if (changes or out != text) else 0
    open(path, "w").write(out)
    print(f"wrote {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
