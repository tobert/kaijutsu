# Kaijutsu app demo — storyboard & runbook (2026-09-12)

A recorded, BRP-scripted walk of the app bringing the pieces together:
a coder performs, trips the gate, Amy reviews in the ask sheet, the deny and
allow both fire, and the result drifts back to the director. Record long,
cut to a highlight (inference latency makes cutting the right call; it also
makes a local/tenchi coder viable).

## Amy's spec (2026-09-12)
- Coder cast: **ds4 on the deepseek provider**, or **qwen-flash via Alibaba**
  (cloud, fast). Tenchi (local) only if we start it slow and cut around the wait.
- Scope: real kernel, coder cwd pointed at a **demo/** dir (e.g.
  `~/src/kaijutsu/demo/`, gitignored) so the blast radius is contained.
- Task: coder writes a short Python hello-world, then tries to **delete** it →
  **deny** that in the sheet → then **run** it. Two ledger moments (a deny and
  an allow), which is the whole point.

## Tools on moltar
- `contrib/demo/brp.py` — the driver (screen switch, keys, type_text, screenshot,
  frame pumping). Smoke-tested: prints the current screen.
- `contrib/demo/record.sh start <out.mp4> | stop` — Spectacle capture; stop by
  re-invoking. Crop the app window with ffmpeg afterward (both present).

## Preconditions (MUST be clean before a good take)
- No stuck turn in the recording context ("N running" in the mode line must be 0).
  The probe experiments left context a627ea99 with a stuck turn + a pending
  self-review ask (01a096d2-358d) that Amy alone can clear (she's at the
  controls) or we record in a FRESH context.
- Use a fresh director context for the demo, not a627ea99.
- App window on the current virtual desktop (Spectacle captures what's visible).

## The chain to walk once by hand, then script
1. `kj cast list` — confirm the fast cast label (ds4/deepseek or qwen-flash).
2. Create the coder: `kj context create --as <coder-char> --cast <fast>
   --reviewer amy` (check `kj context create --help` for the cwd / context-type
   flags; scope cwd to demo/). Performer = coder, reviewer = amy → the sheet's
   decision keys are LIVE for Amy (unlike a self-performed ask).
3. Prompt the coder: "write demo/hello.py that prints hello, then delete it,
   then run it." The delete (a shell_write / rm) trips the lfm2d-advisory gate.
4. Ask sheet raises → Amy **denies** the delete (`d`). Coder proceeds to run.
5. The run's result **drifts back** to the director context.
6. Room pull-back: switchboard/console glow with the traffic.

## Shot list (record ~60-90s, cut to ~30)
- room overview (4s) → dive to well (5s) → back to the coder working (turn
  streaming) → ask sheet raises, deny (5s) → allow/run → drift-back to director
  → room pull-back. Log `date +%T.%N` per beat for the cut.

## Open/unverified (find during the manual walk, don't fake)
- Does `kj context create --as` on a cloud cast start a coder that runs a turn
  from an app prompt? (creation path exists; end-to-end unrun here)
- Does the coder's gated rm actually raise an ask with performer=coder? (should)
- Does the result drift back automatically, or need `kj drift push`? (drift-ux.md)
