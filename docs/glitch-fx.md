# Glitch FX Ideas

## Diagonal Cascade Wave (observed 2026-02-07)

During app startup, the `kj tail` output produces an accidental visual effect:
a wave of color that sweeps diagonally from top-left to bottom-right.

**How it happens:** The tracing format is `[dim timestamp] [green INFO] [dim module_path:] message`.
During initialization, deeper subsystems log in sequence, and their module paths get
progressively longer:

```
kaijutsu_client::ssh                              → message at col ~55
kaijutsu_app::cell::sync                          → col ~59
kaijutsu_app::cell::systems                       → col ~62
kaijutsu_app::ui::constellation                   → col ~68
kaijutsu_app::ui::constellation::mini             → col ~72
kaijutsu_app::ui::constellation::render           → col ~74
kaijutsu_app::ui::constellation::create_dialog    → col ~81
```

The boundary between dim (module path) and bright (message content) shifts right on each
successive line, creating a diagonal sweep. The green `INFO` tag provides a fixed-column
anchor that makes the diagonal more visible against the dim/bright contrast.

**Shader idea:** A "cascade reveal" effect where text appears with a diagonal wavefront
sweeping top-left → bottom-right. Each line's bright region starts a few pixels further
right than the previous. Could be parameterized:
- `wave_angle` — controls the diagonal slope
- `wave_speed` — how fast the wavefront moves
- `dim_alpha` / `bright_alpha` — contrast between revealed and unrevealed text
- `wave_width` — soft vs sharp transition at the wavefront

Could work nicely as a transition effect when switching contexts or loading new content.
The natural "init cascade" appearance gives it an organic, system-coming-alive feel.
