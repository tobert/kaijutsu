#!/usr/bin/env python3
"""Fingerprint a Cap'n Proto schema's meaning, so a pure reorganization can prove itself.

Moving declarations around a `.capnp` file is safe — an ordinal is written `@N`,
so textual position carries no meaning. Reviewing that safety is not safe: a
2000-line move diff hides a changed type or a dropped field as easily as it
shows them, and `capnp compile` accepts plenty of edits that break the wire.

This prints one sorted line per ordinal-bearing member:

    Kernel.getBlocks @35 (query :BlockQuery, trace :TraceContext) -> (...)

Run it before and after a reorganization and diff the two. Identical output
means every member kept its container, its name, its ordinal, and its type —
which is the whole of what the wire promises. Reordered output is not a
difference: the lines are sorted, so only meaning shows up.

What this does NOT check: that the file still compiles (run `capnp compile`),
and struct field ordinals against the *previous released* schema (this compares
two files you give it, not history).

Usage:
    capnp-fingerprint.py <file.capnp>                 print the fingerprint
    capnp-fingerprint.py <a.capnp> --diff <b.capnp>   compare two files
    capnp-fingerprint.py <file.capnp> --self-test     prove the check can fail

--self-test is the negative control. A check nobody has watched fail is a check
nobody has tested, so this mutates one ordinal in memory and asserts the
fingerprint changes. It exits 1 if the mutation slips through.
"""

import re
import sys


def strip_comments(src: str) -> str:
    """Remove `#` comments, leaving string literals intact."""
    out = []
    i = 0
    n = len(src)
    while i < n:
        c = src[i]
        if c == '"':
            out.append(c)
            i += 1
            while i < n:
                out.append(src[i])
                if src[i] == "\\":
                    i += 1
                    if i < n:
                        out.append(src[i])
                        i += 1
                    continue
                if src[i] == '"':
                    i += 1
                    break
                i += 1
            continue
        if c == "#":
            while i < n and src[i] != "\n":
                i += 1
            continue
        out.append(c)
        i += 1
    return "".join(out)


HEADER_NAME = re.compile(
    r"\b(?:struct|interface|enum|union|group)\b\s*([A-Za-z_][A-Za-z0-9_]*)?"
)
FIELD_GROUP = re.compile(r"([A-Za-z_][A-Za-z0-9_]*)\s*(?:@(\d+)\s*)?:\s*(?:group|union)\b")
ORDINAL = re.compile(r"@(\d+)\b")
MEMBER_NAME = re.compile(r"^\s*([A-Za-z_][A-Za-z0-9_]*)\s*@")


def container_label(header: str) -> str:
    """Name a container from the text preceding its `{`."""
    header = header.strip()
    g = FIELD_GROUP.search(header)
    if g:
        return g.group(1)
    m = HEADER_NAME.search(header)
    if m:
        return m.group(1) or "union"
    # An anonymous union is written bare.
    if header.endswith("union"):
        return "union"
    return header.split()[-1] if header.split() else "?"


def fingerprint(src: str) -> list[str]:
    text = strip_comments(src)
    stack: list[str] = []
    buf: list[str] = []
    lines: list[str] = []

    for ch in text:
        if ch == "{":
            header = "".join(buf)
            # A group/union field is both a member and a container; record it
            # as a member too, since it can carry its own ordinal.
            g = FIELD_GROUP.search(header)
            if g and g.group(2) is not None:
                path = ".".join(stack + [g.group(1)])
                lines.append(f"{path} @{g.group(2)} :group")
            stack.append(container_label(header))
            buf = []
        elif ch == "}":
            if stack:
                stack.pop()
            buf = []
        elif ch == ";":
            decl = " ".join("".join(buf).split())
            if ORDINAL.search(decl) and stack:
                name_m = MEMBER_NAME.match(decl)
                name = name_m.group(1) if name_m else decl.split("@")[0].strip()
                path = ".".join(stack + [name])
                # Keep the ordinal and everything after it: that is the type,
                # the parameter list, and the return type — all wire-bearing.
                at = ORDINAL.search(decl)
                lines.append(f"{path} @{at.group(1)}{decl[at.end():]}".rstrip())
            buf = []
        else:
            buf.append(ch)

    return sorted(lines)


def main() -> int:
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    path = sys.argv[1]
    with open(path, encoding="utf-8") as fh:
        src = fh.read()

    if "--self-test" in sys.argv:
        base = fingerprint(src)
        # Mutate exactly one ordinal and require the fingerprint to notice.
        # Mutate the COMMENT-STRIPPED text: the first `@N` in the raw file sits
        # inside a comment, and a comment carries no meaning — mutating it
        # proved only that the self-test was aimed at the wrong bytes.
        stripped = strip_comments(src)
        # `\b` keeps this off the file ID: a schema opens with `@0xd4c9...;`,
        # whose `@0` is not an ordinal. Matching it mutated a hex constant at
        # file scope and proved nothing — the second wrong aim in one tool.
        mutated_src, count = re.subn(
            r"@(\d+)\b", lambda m: f"@{int(m.group(1)) + 1000}", stripped, count=1
        )
        if count != 1:
            print("self-test could not find an ordinal to mutate", file=sys.stderr)
            return 1
        mutated = fingerprint(mutated_src)
        if base == mutated:
            print("SELF-TEST FAILED: an ordinal changed and the fingerprint did not.", file=sys.stderr)
            return 1
        # And a pure reformat must NOT register as a change.
        reflowed = re.sub(r"[ \t]+", " ", src)
        if fingerprint(reflowed) != base:
            print("SELF-TEST FAILED: whitespace alone changed the fingerprint.", file=sys.stderr)
            return 1
        print(f"self-test OK — {len(base)} members; an ordinal change is caught, whitespace is not.")
        return 0

    if "--diff" in sys.argv:
        other = sys.argv[sys.argv.index("--diff") + 1]
        with open(other, encoding="utf-8") as fh:
            other_fp = fingerprint(fh.read())
        this_fp = fingerprint(src)
        only_a = [l for l in this_fp if l not in set(other_fp)]
        only_b = [l for l in other_fp if l not in set(this_fp)]
        if not only_a and not only_b:
            print(f"identical — {len(this_fp)} members, no semantic change")
            return 0
        for l in only_a:
            print(f"-{l}")
        for l in only_b:
            print(f"+{l}")
        print(f"\n{len(only_a)} removed, {len(only_b)} added", file=sys.stderr)
        return 1

    for line in fingerprint(src):
        print(line)
    return 0


if __name__ == "__main__":
    sys.exit(main())
