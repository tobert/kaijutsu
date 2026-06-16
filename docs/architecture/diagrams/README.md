# Architecture diagrams

SVGs referenced by the docs in [`..`](../README.md). Generated via scry
(computational layout) and verified visually before saving; the SVG is the
artifact, editable by re-running the generator.

| File | Shows | Referenced by |
|---|---|---|
| `01-system-topology.svg` | clients → SSH/Cap'n Proto → server → the one shared Kernel + its stores | [README](../README.md#process--transport-model) |
| `02-kernel-anatomy.svg` | persistence & the CRDT journal lifecycle (write → oplog → snapshot → cold-start replay) | [README](../README.md#the-kernel-an-orchestration-hub), [kernel](../kernel.md) |
| `03-context-vs-conversation.svg` | durable multi-writer context vs append-only hydrated conversation; exclude/edit → fork | [README](../README.md#the-data-model-context-vs-conversation) |
| `04-turn-flow.svg` | a turn end-to-end, prompt to pixels, with the agentic loop | [README](../README.md#how-a-turn-flows) |
| `05-mcp-broker.svg` | the single tool-dispatch pipeline; builtin vs external servers | [README](../README.md#tool-dispatch-the-mcp-broker) |
| `06-crate-deps.svg` | workspace dependency layering, leaves up | [README](../README.md#crate-dependency-map) |

All are dark-theme, 1000-px-wide vector. Text uses a
`DejaVu Sans, Liberation Sans, sans-serif` stack so they rasterize in headless
tools and fall back cleanly in browsers.
