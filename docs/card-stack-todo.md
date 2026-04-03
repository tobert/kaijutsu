# Conversation Stack — Next Steps

First pass landed 2026-04-02. Cards render with real block textures in 3D
perspective, navigation works, clean round-trip to Conversation view.

## Immediate (visual quality)

- [ ] **Card size tuning** — focused card should fill ~70% of viewport width.
      Camera is at z=180, card_width=200. Either bring camera closer or
      increase card_width. BRP-tweak `CardStackLayout` live to find good values.
- [x] **Dock mode label** — `mode_label_stack` added to Theme, shows "STACK".
- [x] **Focused card highlight** — smooth focus_blend drives glow intensity
      (0.5→0.9), z-pop (+5), and scale bump (+3%) as card approaches focus.

## Animation

- [x] **Smooth transitions** — `interpolate_stack_focus` provides exponential
      ease toward focused_index. Velocity-based lean on rotation.
- [x] **Entry animation** — `StackAnimPhase::Entering` collapses cards to a
      point, ease_out_cubic spread over ~0.3s. Exit animation deferred (needs
      deferred state transition to keep systems running during exit).

## Custom shader (StackCardMaterial)

- [x] **AsBindGroup working** — packed `StackCardUniforms` struct at binding(2)
      with texture(0)/sampler(1). `stack_card.wgsl` renders edge glow + opacity.
- [ ] **LOD text degradation** — blur → colored bars → dim outline as cards
      recede. Extend the fragment shader.
- [ ] **Holographic edge glow enhancement** — SDF-based chromatic-shifted glow,
      time-animated shimmer. Current edge glow is basic smoothstep.
      Role-colored (cyan/violet/amber).
- [ ] **Back face** — dark with faint wireframe edge when card rotation shows
      the back.

## Interaction

- [ ] **Read-only scroll** — focused card can be vertically panned if the
      content is taller than the card viewport. Mouse wheel or Ctrl+U/D.
- [ ] **Dive in** — Enter on focused card switches to Conversation view
      scrolled to that card's blocks.
- [ ] **Mouse click** — click a visible card to focus it.
- [ ] **Momentum scrolling** — mouse wheel with velocity decay for flicking
      through the stack.

## Architecture

- [ ] **Camera parallax** — subtle camera movement tracking the focused card
      position for depth feel.
- [ ] **Card texture updates** — currently cards are spawned once on
      OnEnter. Streaming blocks (during model response) should update the
      card's child quads. May need a per-frame texture handle refresh system.
- [ ] **Card grouping evolution** — currently role-run grouping. May want
      user-turn + model-response as one "exchange" card, or collapsible
      tool-call groups within a card.
- [ ] **Ambient environment** — subtle particle field or star-field in the
      dark void background. Post-process bloom for the edge glow.

## Tech debt (from this session)

- [ ] **Role group borders** — still Vello, should be shader-drawn like block
      borders. See `tech_debt.md` item #6.
- [x] **Material module active** — `StackCardMaterial` + `stack_card.wgsl` are
      in use. Stale "deferred" comments cleaned up.
