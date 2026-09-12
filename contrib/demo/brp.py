#!/usr/bin/env python3
"""Drive kaijutsu-app over the Bevy Remote Protocol for scripted demos.

BRP is plain JSON-RPC over HTTP on :15702. This wraps the calls the demo
needs and the two gotchas learned on moltar (2026-09-12):
  - an injected key/state change lands one request LATE, so every mutating
    call is followed by a cheap read to flush the frame;
  - the reactive winit loop sleeps, so `settle` pumps reads to advance frames
    instead of relying on wall-clock sleeps.
Recording is Spectacle, started/stopped by re-invoking the same CLI (see
contrib/demo/record.sh) — not driven from here.
"""
import json, sys, time, urllib.request

PORT = 15702
URL = f"http://127.0.0.1:{PORT}/jsonrpc"
SCREEN = "bevy_state::state::resources::State<kaijutsu_app::ui::screen::Screen>"
NEXT = "bevy_state::state::resources::NextState<kaijutsu_app::ui::screen::Screen>"

def rpc(method, params=None):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method,
                       "params": params or {}}).encode()
    req = urllib.request.Request(URL, body, {"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=10) as r:
        out = json.load(r)
    if "error" in out:
        raise RuntimeError(f"{method}: {out['error']}")
    return out.get("result")

def screen():
    return rpc("world.get_resources", {"resource": SCREEN})["value"]

def set_screen(name):
    rpc("world.insert_resources", {"resource": NEXT, "value": {"Pending": name}})
    screen()  # flush the frame

def settle(pumps=8, gap=0.15):
    for _ in range(pumps):
        screen()
        time.sleep(gap)

def keys(names, hold_ms=100):
    rpc("brp_extras/send_keys", {"keys": names, "duration_ms": hold_ms})
    screen()

def chord(*names):           # e.g. chord("ControlLeft","KeyA","KeyL")
    keys(list(names))

def type_text(text):
    rpc("brp_extras/type_text", {"text": text})
    screen()

def shot(path):
    rpc("brp_extras/screenshot", {"path": path})

def hold(seconds):           # advance frames for `seconds` so motion shows
    settle(pumps=max(1, int(seconds / 0.15)), gap=0.15)

if __name__ == "__main__":
    # smoke test: report the current screen
    print("screen:", screen())
