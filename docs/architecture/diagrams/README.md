# Architecture diagrams

SVGs referenced by the docs in [`..`](../README.md). Generated via scry
(computational layout) and verified visually before saving; the SVG is the
artifact, editable by re-running the generator.

| File | Shows | Referenced by |
|---|---|---|
| `02-kernel-anatomy.svg` | persistence & the CRDT journal lifecycle (write → oplog → snapshot → cold-start replay) | [README](../README.md#the-kernel-the-instruments-body), [kernel](../kernel.md) |
| `03-context-vs-conversation.svg` | durable multi-writer context vs append-only hydrated conversation; exclude/edit → fork | [README](../README.md#the-data-model-context-vs-conversation) |
| `04-turn-flow.svg` | a turn end-to-end, prompt to pixels, with the agentic loop | [README](../README.md#how-a-turn-flows) |
| `05-mcp-broker.svg` | the single tool-dispatch pipeline; builtin vs external servers | [README](../README.md#tool-dispatch-the-mcp-broker) |

**Two were deleted on 2026-08-16 rather than corrected.**
`01-system-topology.svg` drew a `Kv` store demolished on 2026-07-04, and
`06-crate-deps.svg` drew a `kaijutsu-crdt` crate that no longer exists, with
three dependency arrows into it. Both errors are structural — boxes and edges,
not labels — so no text edit could fix them, and `scry` is not installed on
zorak. A diagram that asserts a subsystem exists is worse than a missing one:
prose can be skimmed past, a box is read as fact. Regenerate and restore them
when the generator is at hand.

All are dark-theme, 1000-px-wide vector. Text uses a
`DejaVu Sans, Liberation Sans, sans-serif` stack so they rasterize in headless
tools and fall back cleanly in browsers.
