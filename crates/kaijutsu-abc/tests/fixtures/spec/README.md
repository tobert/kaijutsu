# ABC v2.1 Spec-Derived Test Fixtures

Every `*.abc` file in this tree is a code example reproduced **verbatim**
from a fenced code block in the **ABC v2.1 music standard** by Chris
Walshaw. Each is paired with a `*.md` file that records the section it
came from, links to that section on the upstream wiki, and quotes the
prose around the example so the test's intent stays legible without
leaving the editor.

## Upstream source

- **Spec:** <https://abcnotation.com/wiki/abc:standard:v2.1>
- **Author:** Chris Walshaw — <https://chriswalshaw.co.uk>, <https://abcnotation.com/contact>
- **Local cache:** `crates/kaijutsu-abc/docs/abc-spec-cache.md`

The cache is a verbatim HTML→Markdown snapshot taken on 2026-05-25. The
extractor that produced this fixture tree walks that cache, not the
internet — so re-runs are reproducible.

## License

The abc wiki content is licensed under **CC BY-NC-SA 3.0 Unported**
(<https://creativecommons.org/licenses/by-nc-sa/3.0/>). That covers
every `*.abc` file here, the prose quoted in each `*.md` file, and the
cached spec at `docs/abc-spec-cache.md`.

Implications worth being conscious of:

- **Attribution:** every derivative file in this tree references
  `abcnotation.com/wiki/abc:standard:v2.1` and the author. Keep it that
  way.
- **NonCommercial:** the spec content cannot be redistributed for
  commercial purposes under the same license. Most fixtures are short
  syntax fragments well inside fair use, but the four full sample tunes
  under `13-sample-abc-tunes-*/` are substantial reproductions —
  revisit before shipping this crate in a commercial context.
- **ShareAlike:** derivative documentation files in this tree (the
  `*.md` siblings, this README) inherit CC BY-NC-SA 3.0. The Rust test
  code that *consumes* the fixtures is not a derivative work of the
  spec and stays under the crate's normal license.

## Layout

```
spec/
├── README.md                                      # this file
├── 02-abc-files-tunes-and-fragments-…/
│   ├── 01.abc                                     # verbatim spec example
│   ├── 01.md                                      # source URL + section + surrounding prose
│   └── …
├── 04-the-tune-body-4-11-ties-and-slurs/
│   └── …
└── …
```

Directory names encode the section number, section title, subsection
number, and subsection title. Fixtures are numbered within their
subsection in the order they appear in the spec.

## Curation

The extractor pulls every fenced code block, but the spec uses fenced
blocks for non-ABC content too — tables of decoration names, syntax
declarations like `%%pageheight <length>`, HTML embedding examples, and
similar. These were manually pruned because they generate huge volumes
of "Skipping unknown character" warnings without exercising real parser
behaviour.

Current corpus: **107 fixtures across 40 sections**. If you re-run the
extractor against an updated spec cache, expect to repeat the curation
pass — diff the new tree against the old one to find regressions.

## Test runner

`crates/kaijutsu-abc/tests/spec_fixtures.rs` walks this tree, attempts
to parse each `*.abc`, and prints a per-section pass/fail summary. It
is currently diagnostic — it does not assert correctness. As the parser
and AST firm up, add targeted tests beside it that assert specific
expectations on individual fixtures.

## Regenerating

The extractor is intentionally throwaway. To rebuild this tree, write
a script that walks `docs/abc-spec-cache.md`, tracks `## N.` and
`### N.M` headings, collects fenced code blocks, and emits the
`<section-slug>/NN.abc` + `<section-slug>/NN.md` pair shown above.
Keep the attribution boilerplate at the top of each `*.md` so every
derived file points back to the source.
