"""Walk a live score and its musician contexts in the Bevy window."""

import argparse
import math
import sys
import time

import brp


def state():
    return brp.rpc("kaijutsu/state")


def act(name):
    brp.rpc("kaijutsu/act", {"action": name})


def wait_for(read, expected):
    deadline = time.monotonic() + 8
    while read() != expected:
        if time.monotonic() >= deadline:
            raise RuntimeError(f"The app did not reach {expected!r}")
        time.sleep(0.1)


def switch(context):
    if not state()["agent_control"]:
        raise RuntimeError("Agent control is off. Press Ctrl+A then g in Bevy.")
    result = brp.rpc("kaijutsu/switch_context", {"context_id": context})
    wait_for(lambda: state()["active_context_id"], result["context_id"])
    return result["context_id"]


def screen(name):
    if state()["screen"] != name:
        act("GoToHeroic" if name == "Heroic" else "DetachToConversation")
        wait_for(lambda: state()["screen"], name)


def layout():
    return brp.rpc("world.get_resources", {"resource": "kaijutsu_app::view::neo::NeoMode"})["value"]


def set_layout(key, enabled):
    if layout()[key] != enabled:
        act("ToggleNeo" if key == "enabled" else "ToggleConversationPanels")
        wait_for(lambda: layout()[key], enabled)


def select_seat(context_id):
    def performers():
        return state()["heroic"]["performers"]

    wait_for(lambda: context_id in performers(), True)
    act(f"JumpSeat:{performers().index(context_id)}")
    wait_for(lambda: state()["heroic"]["selected"], context_id)


def run(score, musicians, seconds, neo_context=None):
    initial = state()
    if not initial["agent_control"]:
        raise RuntimeError("Agent control is off. Press Ctrl+A then g in Bevy.")
    if initial["screen"] not in ("Conversation", "Heroic") or not initial["active_context_id"]:
        raise RuntimeError("Open a context in Conversation or Heroic before starting the tour.")
    initial_layout = layout()
    initial_seat = (initial.get("heroic") or {}).get("selected")
    chairs = []

    def hold(caption, duration):
        print(caption, flush=True)
        time.sleep(duration)

    try:
        screen("Conversation")
        set_layout("enabled", False)
        set_layout("panels_open", False)
        switch(score)
        act("ScrollToEnd")
        hold(f"Track score — {score}", seconds)
        for musician in musicians:
            chairs.append((musician, switch(musician)))
            act("ScrollToEnd")
            hold(f"Musician's latest turn — {musician}", seconds / 2)
        if neo_context:
            switch(neo_context)
            set_layout("enabled", True)
            hold(f"Score and dialogue in Neo — {neo_context}", seconds)
            set_layout("enabled", False)

        screen("Heroic")
        hold("Live transport in Heroic", seconds)
        for musician, context_id in chairs:
            select_seat(context_id)
            hold(f"Heroic — {musician}", seconds * 1.5)
    finally:
        if state()["agent_control"]:
            screen("Conversation")
            switch(initial["active_context_id"])
            set_layout("enabled", False)
            set_layout("panels_open", initial_layout["panels_open"])
            set_layout("enabled", initial_layout["enabled"])
            screen(initial["screen"])
            if initial_seat:
                select_seat(initial_seat)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("score", help="Score context label or ID")
    parser.add_argument("musicians", nargs="*", help="Musician contexts in display order")
    parser.add_argument("--port", type=int, default=brp.PORT, help="App BRP port (default 15702)")
    parser.add_argument("--seconds", type=float, default=6, help="Seconds on the score and Heroic overview (default 6)")
    parser.add_argument("--neo-context", help="Include a Neo view of this context")
    args = parser.parse_args()
    if not math.isfinite(args.seconds) or args.seconds <= 0:
        parser.error("--seconds must be a finite positive number")
    brp.URL = f"http://127.0.0.1:{args.port}/jsonrpc"
    try:
        run(args.score, args.musicians, args.seconds, args.neo_context)
    except (RuntimeError, OSError, KeyboardInterrupt) as error:
        sys.exit(str(error) or "Tour interrupted")
