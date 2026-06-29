# 脈 myaku — retired (folded into tracks + shared-state)

> **Status:** retired 2026-06-29. myaku as a *standalone pulse facility* is
> superseded by the **beat-on-track** model. It was a workaround for the beat
> being welded to the musician's transport (hence "one executor, two trigger
> front-ends" so metrics could keep sampling while music paused). With the beat
> living on the **track**, that whole tension is just *two tracks* — a
> system-clock metrics track you never stop, and a musical track you do.

myaku split cleanly and stopped being a facility:

- **cadence, fire-coordinate injection (`KJ_TICK`/`KJ_PULSE`/`KJ_EPOCH_NS`), and
  the death certificate** → are what a **track** does. See **`docs/tracks.md`**.
- **the `/run` output substrate (`now`/`history`/`status`) and the `pulse_emit`
  kaish helper** → are what a probe *attachment writes*, and belong to
  **`docs/shared-state.md`** (the VFS-is-the-namespace thesis).

A probe is then just: *a context attached to a system-clock track, whose tick
behaviour writes `/run`.* No facility, no second scheduler.

The detailed standalone design (the `pulse_emit` surface, the `/run/pulse/<x>/`
layout, the `KJ_` coordinate table, the "cron + a death certificate" framing)
lived here and remains in **git history** — pull it forward when the `/run`
output substrate is written up in `shared-state.md`. It was good work; it just
belongs in two other docs now.
