# VFS Landscape — Station Design (fsn)

2026-07-07. Station #5 of the scenes charter (`docs/scenes/README.md`).
Earlier-stage than `patchbay.md` — direction and primitives are decided;
interaction UX still needs a concepting wave before slices get cut.

**Lineage** (Amy, wave 2): **fsn**, SGI's experimental 3D file browser for
IRIX — the "It's a UNIX system! I know this!" scene in Jurassic Park. A
flying view over a dark plane where directories are pedestals, files are
blocks standing on them, and luminous avenues connect parent to child.
The homage is deliberate; the palette is ours (Arcane Techmage). Canonical
concepts: `docs/scenes/concepts/43-vfs-fsn-landscape.png` (the landscape),
`44-vfs-fsn-node.png` (one directory up close), `46-vfs-fsn-diorama-vr.png`
(kept for its LOD-as-interaction reading — the pinch-to-upgrade moment,
minus the hand per the charter's presence rule).

## What the station shows

The kernel VFS as terrain:

- **Directories are platforms; files are slabs standing on them**;
  parent→child avenues of light branch across a grid-etched plane. You fly
  it / jump it; you do not walk.
- **Ownership zones are lighting**: CRDT-owned regions (config, `/etc/rc`)
  glow warm amber — alive, editable, kernel-truth; mounted read-only
  regions sit cool cyan behind faint glass (`read_only_shell` made
  visible). Zone boundaries follow the mount table (`docs/mounts.md`,
  `docs/config-crdt-ownership.md`).
- **rc scripts are objects with visible state**: `kj rc list` already knows
  `[in-sync]` / `[differs from seed]` — staleness renders as a material
  lane on the slab (a seam of off-color light), not a label. Symlinks
  (`DocKind::Symlink`, the init.d-style rc composition) render as ghost
  slabs tethered to their targets.
- Unlike the patch bay (whose ground truth is split kernel/edge), the VFS
  is the **pure case of the charter principle**: the scene renders
  kernel-owned state and nothing else. Whatever the VFS surface can
  enumerate, the landscape can show.

## LOD is a design primitive here, not an optimization

The charter finding (wave 2), owned by this station because its population
is unbounded: **near is rich, far is cheap, approach upgrades.**

- Tiers echo the time well's (card → chip → point): near platforms render
  full slabs with labels and state lanes; mid-distance platforms are
  simple glowing blocks; far ones are points on the grid. Same instancing
  strategy as the well's aspirational tiers (`MeshTag` + shared material).
- Detail upgrade is driven by camera distance *and* by attention: the
  selected platform blooms regardless of distance (mockup 46's moment).
- Budget rule inherited from the charter: the landscape must idle cheap on
  a battery laptop — the grid, avenues, and far tiers are the whole scene
  until the player moves.

## In the shell

The landscape is the type specimen for the shell's **archway** pattern
(`docs/scenes/shell.md`): too big to be furniture, so the north bearing is
a doorway showing a live glimpse — horizon glow tracks VFS churn (writes,
rc edits, mounts appearing). Diving steps through the arch onto the plane.

## Interaction sketch (to be concepted, not yet designed)

- **Travel by intent**: jump to a path (type it, or follow an avenue);
  the camera flies the route so the tree's shape registers.
- **Select** a slab → floating hologram preview (the existing file read
  surfaces); **dive** → the vi editor session on that file (`docs/vi.md`)
  — the landscape is plausibly vi's spatial front door.
- The overview/diorama reading (46) may become the dive's entry framing:
  arrive above the whole plane, then descend. Undecided.

## Open questions

1. **What does slab height encode?** fsn used file size; ours are mostly
   small text docs. Candidates: block count for CRDT docs, recency, or
   nothing (uniform slabs, let zones and state lanes carry meaning).
   Decide against real data when slice 0 renders the real tree.
2. **Enumeration surface**: what the scene reads to build the terrain —
   the VFS backends can list, but the app needs a listing RPC shaped for
   incremental sync (the well's poll/diff pattern). Additive wire work;
   size it when slicing.
3. Do zones follow the mount table exactly, or does `/etc` deserve its own
   named district regardless of mount shape?
4. Search: `/` over the landscape (labels/paths, maybe `search_similar`) —
   flying to results vs teleporting.
5. Where the shadow-mount gotcha surfaces: kaish's `/v/blobs` overlay
   shadowing (auto-memory `gotcha_kaish_v_blobs_shadow`) is exactly the
   kind of truth-split the landscape must not paper over — if two surfaces
   disagree, that's a rendering decision (show the seam), not a bug to
   hide.

## Status

Concepting. Next wave for this station: in-scene UX — selection, preview
hologram, path breadcrumb (edge-HUD grammar), zone boundary treatment —
before any slices are cut. Slice 0 sketch when ready: render the real
tree read-only from a snapshot listing, LOD tiers live, no interaction
beyond fly + select.
