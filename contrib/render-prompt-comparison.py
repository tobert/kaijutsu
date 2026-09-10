#!/usr/bin/env python3
"""Compare a pinned prompt baseline with shipped seed literals; never run rc."""

from pathlib import Path
import argparse
import difflib
import hashlib
import html
import json
import re


ROOT = Path(__file__).resolve().parents[1]
DOCS = ROOT / "docs"


def read(relative):
    return (ROOT / relative).read_text()


def one(pattern, text):
    matches = re.findall(pattern, text, re.S | re.M)
    if len(matches) != 1:
        raise ValueError(f"Expected one match for {pattern!r}, found {len(matches)}")
    return matches[0]


def diff(left, right):
    a, b = re.findall(r"\s+|\S+", left), re.findall(r"\s+|\S+", right)
    old, new = [], []
    for tag, i, j, k, l in difflib.SequenceMatcher(None, a, b, autojunk=False).get_opcodes():
        x, y = html.escape("".join(a[i:j])), html.escape("".join(b[k:l]))
        old.append(x if tag == "equal" else f'<mark class="removed">{x}</mark>')
        new.append(y if tag == "equal" else f'<mark class="added">{y}</mark>')
    return ["".join(old), "".join(new)]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="Fail if the saved HTML differs from a fresh render; write nothing")
    args = parser.parse_args()
    baseline_path = "contrib/prompt-comparison-before.json"
    baseline = json.loads(read(baseline_path))
    base_path = "assets/defaults/rc/lib/create/S00-base.md"
    coder_path = "assets/defaults/rc/coder/create/S00-stance.kai"
    general_path = "assets/defaults/rc/default/create/S00-stance.md"
    distill_path = "assets/defaults/prompts/distillation.md"
    continuation_path = "assets/defaults/prompts/continuation.md"
    shared_links = ["assets/defaults/rc/coder/create/S00-base.md",
                    "assets/defaults/rc/default/create/S00-base.md"]
    for link in shared_links:
        if read(link).strip() != "/config/rc/lib/create/S00-base.md":
            raise ValueError(f"Shared prompt selection changed in {link}; update the comparison")
    script = read(coder_path)
    core_matches = re.findall(r'^core="(.*?)"$', script, re.S | re.M)
    if core_matches:
        core = one(r'^core="(.*?)"$', script)
        suffixes = re.findall(r'^    stance="\$core(.*?)"$', script, re.S | re.M)
        if len(suffixes) != 2 or any("$" in part or "\\" in part for part in [core, *suffixes]):
            raise ValueError("Coder string shape changed; update extraction before rendering")
        focused, guided = [core + suffix for suffix in suffixes]
    else:
        stances = re.findall(r'^    stance="(.*?)"$', script, re.S | re.M)
        if len(stances) != 2 or any("$" in part or "\\" in part for part in stances):
            raise ValueError("Coder string shape changed; update extraction before rendering")
        focused, guided = stances
    shipped = dict(base=read(base_path).strip(), focused=focused, guided=guided,
                   general=read(general_path).strip(), drift=read(distill_path).strip(),
                   handoff=read(continuation_path).strip())
    titles = ["Base", "Coder focused", "Coder guided", "General purpose", "Drift briefing", "Compact fork handoff"]
    specs = [
        ("base", base_path, "Before: automatic for every type. After: an optional shared rc file.",
         "Coder and default choose the shared body through rc symlinks. Other types use their own instructions without a kernel-wide behavioral prepend.", "direction-for-base-coder-and-general-purpose-contexts"),
        ("focused", coder_path, "The focused branch of the shipped coder script.",
         "Makes TDD explicit and removes the promise that a context fork makes file edits risk-free. Existing model-tier selection is retained; it is not a measured capability ranking.", "what-its-behavioral-prompt-has-learnedand-where-it-conflicts"),
        ("guided", coder_path, "The guided branch of the shipped coder script.",
         "Keeps a numbered coding procedure with evidence and verification. It shares the same optional working contract as the focused branch.", "direction-for-base-coder-and-general-purpose-contexts"),
        ("general", general_path, "Before: default had no dedicated role text. After: general-purpose guidance.",
         "Default handles research, explanation, writing, planning, and practical tasks. Assistant remains fleet coordination.", "goose"),
        ("drift", distill_path, "Briefing for another context, with rc-configurable word guidance.",
         "The formatter now supplies source and tool references, honors exclusions, and bounds complete turn groups instead of cutting block tails.", "deepseek-harness-prompt-contribution-and-cache-aware-summaries"),
        ("handoff", continuation_path, "A separate continuation instruction for compact forks.",
         "Compact forks retain chosen instruction blocks and the latest complete turn. This handoff preserves active work, corrections, evidence status, and recovery references.", "compaction-retained-history-and-recoverable-content"),
    ]
    records = []
    for title, (key, source, note, why, anchor) in zip(titles, specs):
        current, proposed = baseline["texts"][key], shipped[key]
        combinable = key in {"focused", "guided", "general"}
        combined_old = "\n\n".join(x for x in [baseline["texts"]["base"], current] if x) if combinable else current
        combined_new = "\n\n".join([shipped["base"], proposed]) if combinable else proposed
        old_source = f'https://github.com/tobert/kaijutsu/blob/{baseline["revision"]}/{baseline["paths"][key]}'
        records.append(dict(id=key, title=title, current=current, proposed=proposed,
                            source=old_source, newSource="../" + source, note=note, why=why, anchor=anchor,
                            combinable=combinable, combinedOld=combined_old, combinedNew=combined_new,
                            diff=diff(current, proposed), combinedDiff=diff(combined_old, combined_new)))
    inputs = [baseline_path, base_path, coder_path, general_path, distill_path, continuation_path,
              *shared_links, "contrib/prompt-comparison.html", "contrib/render-prompt-comparison.py"]
    sources = {p: hashlib.sha256(read(p).encode()).hexdigest() for p in inputs}
    payload = json.dumps(dict(records=records, sources=sources), ensure_ascii=False).replace("<", "\\u003c")
    template = (ROOT / "contrib/prompt-comparison.html").read_text()
    if template.count("__PROMPT_DATA__") != 1:
        raise ValueError("Expected exactly one data slot in HTML template")
    output = DOCS / "prompt-comparison.html"
    rendered = template.replace("__PROMPT_DATA__", payload)
    if args.check:
        if not output.exists() or output.read_text() != rendered:
            parser.exit(1, "Prompt comparison is stale or missing; run python3 contrib/render-prompt-comparison.py\n")
        print(f"Current: {output}")
        return
    output.write_text(rendered)
    print(f"Rendered {len(records)} comparisons to {output}")


if __name__ == "__main__":
    main()
