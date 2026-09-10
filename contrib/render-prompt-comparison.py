#!/usr/bin/env python3
"""Render the prompt review from seed literals and Markdown drafts; never run rc."""

from pathlib import Path
import difflib
import hashlib
import html
import json
import re
import subprocess


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
    base_path = "assets/defaults/system.md"
    coder_path = "assets/defaults/rc/coder/create/S00-stance.kai"
    distill_path = "assets/defaults/prompts/distillation.md"
    base = read(base_path).strip()
    script = read(coder_path)
    core = one(r'^core="(.*?)"$', script)
    suffixes = re.findall(r'^    stance="\$core(.*?)"$', script, re.S | re.M)
    if len(suffixes) != 2 or any("$" in s or "\\" in s for s in [core, *suffixes]):
        raise ValueError("Coder string shape changed; update extraction before rendering")
    focused, guided = [core + suffix for suffix in suffixes]
    distill = read(distill_path).strip()
    drafts = dict(re.findall(r'^## ([^\n]+)\n.*?^```text\n(.*?)\n```',
                             read("docs/prompt-proposals.md"), re.S | re.M))
    titles = ["Base", "Coder focused", "Coder guided", "General purpose", "Drift briefing", "Compact fork handoff"]
    if set(drafts) != set(titles):
        raise ValueError(f"Draft sections changed: {list(drafts)}")
    default_dir = ROOT / "assets/defaults/rc/default/create"
    if any("stance" in p.name for p in default_dir.iterdir()):
        raise ValueError("Default now has a stance; review its current body")
    specs = [
        ("base", base, base_path, "Shared by all context types.",
         "Adds intent continuity and grounded reports to our shared stance. Review the cost to every type, including musician and MCP.", "direction-for-base-coder-and-general-purpose-contexts"),
        ("focused", focused, coder_path, "Current focused branch: shared core + focused suffix.",
         "Moves common behavior into the base, makes TDD explicit, and removes the promise that a context fork makes file edits risk-free.", "what-its-behavioral-prompt-has-learnedand-where-it-conflicts"),
        ("guided", guided, coder_path, "Current guided branch: shared core + guided suffix.",
         "Retains a numbered coding procedure. Both variants are drafts to compare; model-name branching has not been validated by this review.", "direction-for-base-coder-and-general-purpose-contexts"),
        ("general", "", "assets/defaults/rc/default/create", "No dedicated stance in default today; the shared base still applies.",
         "Tries general work under default. The existing assistant is fleet coordination. Compare against base alone before deciding this role earns its text.", "goose"),
        ("drift", distill, distill_path, "Current shared distillation instruction; also used by compact forks.",
         "Keeps a brief for a recipient, with evidence and uncertainty. Missing block references must be supplied by the formatter, not invented by the model.", "deepseek-harness-prompt-contribution-and-cache-aware-summaries"),
        ("handoff", distill, distill_path, "Compact fork currently reuses this drift briefing instruction.",
         "Proposes a separate continuation prompt: active objective, corrections, evidence, pending questions, and recovery references. Requires later runtime wiring and retention tests.", "compaction-retained-history-and-recoverable-content"),
    ]
    records = []
    for title, (key, current, source, note, why, anchor) in zip(titles, specs):
        proposed = drafts[title]
        combinable = key in {"focused", "guided", "general"}
        combined_old = "\n\n".join(x for x in [base, current] if x) if combinable else current
        combined_new = "\n\n".join([drafts["Base"], proposed]) if combinable else proposed
        records.append(dict(id=key, title=title, current=current, proposed=proposed,
                            source="../" + source, note=note, why=why, anchor=anchor,
                            combinable=combinable, combinedOld=combined_old, combinedNew=combined_new,
                            diff=diff(current, proposed), combinedDiff=diff(combined_old, combined_new)))
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    sources = {p: hashlib.sha256(read(p).encode()).hexdigest() for p in [base_path, coder_path, distill_path]}
    payload = json.dumps(dict(records=records, revision=revision, sources=sources), ensure_ascii=False).replace("<", "\\u003c")
    template = (ROOT / "contrib/prompt-comparison.html").read_text()
    if template.count("__PROMPT_DATA__") != 1:
        raise ValueError("Expected exactly one data slot in HTML template")
    output = DOCS / "prompt-comparison.html"
    output.write_text(template.replace("__PROMPT_DATA__", payload))
    print(f"Rendered {len(records)} comparisons to {output}")


if __name__ == "__main__":
    main()
