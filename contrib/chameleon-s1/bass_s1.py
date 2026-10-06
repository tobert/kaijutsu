#!/usr/bin/env python3
"""Play a bass line from System 1 decisions on the megakernel, one window of bars per decision.

Usage:
  python3 bass_s1.py [--chart charts/chameleon.json] [--key D --mode dorian --changes "Dm7 G7"]
                     [--shape plan|form|per_bar] [--window 8] [--windows 2]
                     [--temperature 1.8] [--seed 1] [--policy sample|argmax] [--log run.jsonl]
  python3 bass_s1.py --compare [--chart ...]      # every shape at 4, 8 and 16 bars
  python3 bass_s1.py --replay run.jsonl [--temperature 1.0] [--seed 2]
  python3 bass_s1.py --print-spec form
  python3 bass_s1.py --render-all [--chart ...]   # every cell on every chord, no server

A chart names the key, the mode, the meter, and the changes (one chord per
bar, repeating). Cells are written relative to each chord's root and spelled
in the chart's key, so any key works.

Each decision reads one case (the window's bars and chords, and how the last
window ended) after one held context (the playing guidance, then the chart)
through one spec. Three spec shapes:

  per_bar  one choice question per bar of the window
  form     four questions: a form over the window's quarters (AAAA, AAAB,
           AABA, ABAB), a groove cell, a contrast cell, and an ending
  plan     one question whose options are whole plans: a groove for the
           window and an ending for its last bar

The council reads non-text questions independently, so every answer is
sampled on its own; the client renders the window from the answers.

Load rules: requests are sequential with --pause seconds between them,
contexts are persist:false, the contexts and specs are deleted at the end,
and any HTTP error stops the run. The server is the megakernel on zorak,
shared with live gate traffic.
"""
import argparse, json, math, random, statistics, sys, time, urllib.error, urllib.request, uuid
from pathlib import Path

BASE = "http://zorak:8090/council/v1"
HERE = Path(__file__).resolve().parent

# ---------------------------------------------------------------- the palette

CELLS = [
    ("root_fifth", "root then fifth, two half notes: the steady default"),
    ("hold", "the root held for the whole bar: a floor under a busy band"),
    ("riff", "the syncopated funk figure: root, root again, a rest, then fifth and seventh"),
    ("pump", "eighth notes jumping between the root and its octave: high energy"),
    ("arpeggio", "root, third, fifth, seventh in quarter notes, climbing"),
    ("walk", "four quarter notes that step through the scale to the next chord's root"),
    ("approach", "the root, then a half-step pickup into the next chord's root"),
    ("space", "one short root on the downbeat, then rest: leave room after a busy bar"),
]
CELL_NAMES = [c for c, _ in CELLS]
FORMS = [
    ("AAAA", "the groove cell the whole window"),
    ("AAAB", "the groove for three quarters of the window, the contrast cell in the last quarter"),
    ("AABA", "groove, groove, contrast, groove, one quarter of the window each"),
    ("ABAB", "groove and contrast alternating by quarters of the window"),
]
ENDINGS = [
    ("walk", "the last bar walks into the next window"),
    ("approach", "the last bar ends on a half-step pickup into the next window"),
    ("space", "the last bar drops out to one short root, leaving room"),
    ("pump", "the last bar pumps eighth notes to drive into the next window"),
    ("none", "no turnaround: the last bar plays its quarter's cell"),
]
PLAN_GROOVES = ["root_fifth", "riff", "hold", "pump", "arpeggio"]
PLAN_ENDINGS = ["walk", "approach", "space", "none"]

GUIDANCE_SYSTEM = (
    "You are the bass player in a funk band. You choose what the bass plays from "
    "a menu of cells, one cell per bar. The band's chart comes next.")
GUIDANCE = (
    "How this bass chair plays:\n"
    "- The bass is the floor the band stands on. Steady beats clever.\n"
    "- Most of a window is one steady groove: root_fifth, riff, or hold. The riff is the signature.\n"
    "- The last bar of a window leads into the next one: walk or approach, sometimes space.\n"
    "- pump and arpeggio add energy. They work as a contrast, not for a whole window.\n"
    "- space leaves room after a busy stretch.\n"
    "- The first bar of a window lands it: play the root solidly there.")

# ---------------------------------------------------------------- music theory

LETTERS = "CDEFGAB"
NATURAL = {"C": 0, "D": 2, "E": 4, "F": 5, "G": 7, "A": 9, "B": 11}
MODES = {  # intervals from the tonic, and the ABC K: suffix
    "ionian": ([0, 2, 4, 5, 7, 9, 11], ""), "major": ([0, 2, 4, 5, 7, 9, 11], ""),
    "dorian": ([0, 2, 3, 5, 7, 9, 10], " dor"), "phrygian": ([0, 1, 3, 5, 7, 8, 10], " phr"),
    "lydian": ([0, 2, 4, 6, 7, 9, 11], " lyd"), "mixolydian": ([0, 2, 4, 5, 7, 9, 10], " mix"),
    "aeolian": ([0, 2, 3, 5, 7, 8, 10], "m"), "minor": ([0, 2, 3, 5, 7, 8, 10], "m"),
    "locrian": ([0, 1, 3, 5, 6, 8, 10], " loc"),
}
QUALITIES = {  # third, fifth, seventh (None: use the octave) in semitones above the root
    "": (4, 7, None), "maj": (4, 7, None), "maj7": (4, 7, 11), "M7": (4, 7, 11), "6": (4, 7, 9),
    "m": (3, 7, None), "min": (3, 7, None), "m7": (3, 7, 10), "min7": (3, 7, 10), "m6": (3, 7, 9),
    "7": (4, 7, 10), "9": (4, 7, 10), "13": (4, 7, 10),
    "m7b5": (3, 6, 10), "dim": (3, 6, None), "dim7": (3, 6, 9),
    "sus4": (5, 7, None), "7sus4": (5, 7, 10),
}
ROOT_LOW = 36  # roots sit in C2..B2 (MIDI 36..47); every note stays below middle C

def parse_pitch_name(name):
    if not name or name[0].upper() not in NATURAL:
        sys.exit(f"cannot read the note name {name!r}: expected a letter A-G, then b or #")
    alter = {"": 0, "b": -1, "#": 1}.get(name[1:])
    if alter is None:
        sys.exit(f"cannot read the note name {name!r}: expected a letter A-G, then b or #")
    return name[0].upper(), alter

def parse_chord(sym):
    n = 2 if len(sym) > 1 and sym[1] in "b#" else 1
    letter, alter = parse_pitch_name(sym[:n])
    quality = sym[n:]
    if quality not in QUALITIES:
        sys.exit(f"chord {sym!r}: unknown quality {quality!r}; known: {', '.join(repr(q) for q in QUALITIES)}")
    pc = (NATURAL[letter] + alter) % 12
    return {"symbol": sym, "root": ROOT_LOW + (pc - ROOT_LOW) % 12, "tones": QUALITIES[quality]}

class Key:
    """A key and mode: its scale, its signature, and how to spell any MIDI pitch in it."""
    def __init__(self, tonic, mode):
        if mode not in MODES:
            sys.exit(f"unknown mode {mode!r}; known: {', '.join(MODES)}")
        letter, alter = parse_pitch_name(tonic)
        steps, self.suffix = MODES[mode]
        self.tonic_name = letter + {-1: "b", 0: "", 1: "#"}[alter]
        tonic_pc = (NATURAL[letter] + alter) % 12
        i0 = LETTERS.index(letter)
        self.spelling, self.signature = {}, {}
        for i, step in enumerate(steps):
            l = LETTERS[(i0 + i) % 7]
            pc = (tonic_pc + step) % 12
            a = (pc - NATURAL[l] + 6) % 12 - 6
            if abs(a) > 1:
                sys.exit(f"key {tonic} {mode} needs a double accidental on {l}; pick its enharmonic key")
            self.spelling[pc], self.signature[l] = (l, a), a
        self.scale = set(self.spelling)
        self.flats = sum(self.signature.values()) < 0 or (sum(self.signature.values()) == 0 and alter < 0)

    def header(self):
        return f"K:{self.tonic_name}{self.suffix}"

    def spell(self, midi):
        pc = midi % 12
        by_pc = {v: k for k, v in NATURAL.items()}
        if pc in self.spelling:
            l, a = self.spelling[pc]
        elif pc in by_pc:
            l, a = by_pc[pc], 0
        elif self.flats:
            l, a = by_pc[(pc + 1) % 12], -1
        else:
            l, a = by_pc[(pc - 1) % 12], 1
        return l, a, midi - a  # letter, alteration, the letter's natural MIDI pitch

class Bar:
    """Writes one bar of ABC. An accidental is written when the pitch differs from the key, or
    when its letter was already altered in this bar, so readers that carry accidentals per
    letter and readers that carry them per octave hear the same pitch."""
    def __init__(self, key):
        self.key, self.touched, self.out = key, set(), []
    def note(self, midi, eighths):
        l, a, nat = self.key.spell(midi)
        acc = ""
        if a != self.key.signature[l] or l in self.touched:
            acc = {-1: "_", 0: "=", 1: "^"}[a]
            self.touched.add(l)
        octave = nat // 12 - 5  # 0 for the octave from middle C
        name = l + ("," * -octave if octave < 0 else "") if octave <= 0 else l.lower() + "'" * (octave - 1)
        self.out.append((f"{acc}{name}", eighths))
    def rest(self, eighths):
        self.out.append(("z", eighths))
    def text(self):
        return " ".join(t + (str(n) if n != 1 else "") for t, n in self.out)
    def eighths(self):
        return sum(n for _, n in self.out)

def walk_line(key, root, target):
    """Root, then three notes that step to target, the next bar's root: scale tones nearest the
    target, then its chromatic neighbor when the scale gives fewer than three."""
    if target == root:  # the same chord again: run up the scale and fall back to the root
        return [root] + [p for p in range(root + 1, root + 12) if p % 12 in key.scale][:3]
    step = 1 if target > root else -1
    between = [p for p in range(root + step, target, step) if p % 12 in key.scale]
    if len(between) < 3 and target - step not in between:
        between.append(target - step)
    line = between[-3:]
    while len(line) < 3:
        line.insert(0, root)
    return [root] + line

def render(cell, chord, next_chord, key):
    R = chord["root"]
    third, fifth, seventh = chord["tones"]
    seven = R + (seventh if seventh is not None else 12)
    b = Bar(key)
    if cell == "root_fifth":
        b.note(R, 4); b.note(R + fifth, 4)
    elif cell == "hold":
        b.note(R, 8)
    elif cell == "riff":
        b.note(R, 3); b.note(R, 1); b.rest(2); b.note(R + fifth, 1); b.note(seven, 1)
    elif cell == "pump":
        for _ in range(4):
            b.note(R, 1); b.note(R + 12, 1)
    elif cell == "arpeggio":
        for p in (R, R + third, R + fifth, seven):
            b.note(p, 2)
    elif cell == "walk":
        for p in walk_line(key, R, next_chord["root"]):
            b.note(p, 2)
    elif cell == "approach":
        target = next_chord["root"]
        b.note(R, 6); b.note(target + 1 if target < R else target - 1, 2)
    elif cell == "space":
        b.note(R, 2); b.rest(6)
    else:
        sys.exit(f"no cell named {cell!r}")
    if b.eighths() != 8:
        sys.exit(f"cell {cell} on {chord['symbol']} fills {b.eighths()} eighths, not 8: {b.text()}")
    return b.text()

# ---------------------------------------------------------------- chart

def load_chart(args):
    chart = json.loads(Path(args.chart).read_text()) if args.chart else {}
    for field in ("key", "mode", "meter", "title"):
        if getattr(args, field, None):
            chart[field] = getattr(args, field)
    if args.changes:
        chart["changes"] = args.changes.split()
    missing = [f for f in ("key", "mode", "changes") if not chart.get(f)]
    if missing:
        sys.exit(f"the chart needs {', '.join(missing)}: give --chart FILE or --key, --mode and --changes")
    chart.setdefault("meter", "4/4")
    chart.setdefault("title", "vamp")
    if chart["meter"] != "4/4":
        sys.exit(f"meter {chart['meter']}: the cells are written for 4/4 only")
    chart["_key"] = Key(chart["key"], chart["mode"])
    chart["_chords"] = [parse_chord(c) for c in chart["changes"]]
    return chart

def chart_text(chart):
    return (f"The chart: {chart['title']}. Key {chart['key']} {chart['mode']}. Meter {chart['meter']}. "
            f"Changes, one chord per bar, repeating: {' | '.join(chart['changes'])}.")

def chord_at(chart, bar):
    return chart["_chords"][bar % len(chart["_chords"])]

# ---------------------------------------------------------------- specs

def spec_for(shape):
    cells = [{"option": c, "means": m} for c, m in CELLS]
    base = {"instructions": "Decide what the bass plays in the next window of bars, from what this "
                            "conversation says about the band, the chart, and the groove.",
            "input_label": "Next window"}
    if shape == "per_bar":
        qs = [{"id": f"bar{i}", "type": "choice", "instructions": f"Which cell the bass plays in bar {i} of the window.",
               "criteria": cells} for i in range(1, 17)]
    elif shape == "form":
        qs = [{"id": "form", "type": "choice", "instructions": "The form of the window, by quarters.",
               "criteria": [{"option": f, "means": m} for f, m in FORMS]},
              {"id": "groove", "type": "choice", "instructions": "The groove cell: what A plays.", "criteria": cells},
              {"id": "contrast", "type": "choice", "instructions": "The contrast cell: what B plays.", "criteria": cells},
              {"id": "ending", "type": "choice", "instructions": "What the last bar of the window plays.",
               "criteria": [{"option": e, "means": m} for e, m in ENDINGS]}]
    elif shape == "plan":
        crit = []
        for g in PLAN_GROOVES:
            for e in PLAN_ENDINGS:
                end = {"none": "and the last bar too", "walk": "then walk into the next window in the last bar",
                       "approach": "then a half-step pickup into the next window in the last bar",
                       "space": "then drop to one short root in the last bar"}[e]
                crit.append({"option": f"{g}_{e}", "means": f"{g} every bar, {end}"})
        qs = [{"id": "plan", "type": "choice", "instructions": "The plan for the whole window.", "criteria": crit}]
    else:
        sys.exit(f"no shape named {shape!r}")
    return {"name": f"proto-bass-{shape.replace('_', '-')}", **base, "questions": qs}

def ask_for(shape, window):
    return [f"bar{i}" for i in range(1, window + 1)] if shape == "per_bar" else None

# ---------------------------------------------------------------- the case and the window

def case(chart, start, window, last):
    chords = " ".join(chord_at(chart, start + i)["symbol"] for i in range(window))
    after = chord_at(chart, start + window)["symbol"]
    prev = f"The last window ended with {last}." if last else "This is the first window."
    return (f"A window of {window} bars, bars {start + 1} to {start + window} of the tune. "
            f"Chords by bar: {chords}. After the window comes {after}. {prev}")

def sample(probs, temperature, policy, rng):
    """Pick an option from {option: p}. argmax ignores temperature; ties go to the earlier option."""
    opts = list(probs)
    if policy == "argmax":
        return max(opts, key=lambda o: probs[o])
    w = [math.exp(math.log(max(probs[o], 1e-12)) / temperature) for o in opts]
    return rng.choices(opts, weights=w)[0]

def cells_for(shape, window, answers, args, rng):
    """The window's cells, one per bar, and the choices that made them."""
    pick = lambda q: sample(answers[q]["probabilities"], args.temperature, args.policy, rng)
    if shape == "per_bar":
        cells = [pick(f"bar{i}") for i in range(1, window + 1)]
        return cells, {"cells": " ".join(cells)}
    if shape == "form":
        form, groove, contrast, ending = pick("form"), pick("groove"), pick("contrast"), pick("ending")
        cells = [groove if form[i * 4 // window] == "A" else contrast for i in range(window)]
        if ending != "none":
            cells[-1] = ending
        return cells, {"form": form, "groove": groove, "contrast": contrast, "ending": ending}
    plan = pick("plan")
    groove, ending = plan.rsplit("_", 1)
    cells = [groove] * window
    if ending != "none":
        cells[-1] = ending
    return cells, {"plan": plan}

def coherence(cells):
    """Sensors for a window: distinct cells, changes between neighbors, and a turnaround at the end."""
    changes = sum(1 for a, b in zip(cells, cells[1:]) if a != b)
    return {"distinct": len(set(cells)), "changes": changes, "turnaround": cells[-1] in ("walk", "approach")}

def render_window(chart, start, cells):
    return [render(c, chord_at(chart, start + i), chord_at(chart, start + i + 1), chart["_key"])
            for i, c in enumerate(cells)]

def abc(chart, bars):
    lines = [" | ".join(bars[i:i + 4]) + " |" for i in range(0, len(bars), 4)]
    return f"X:1\nT:bass\nM:{chart['meter']}\nL:1/8\n{chart['_key'].header()}\n" + "\n".join(lines) + "\n"

# ---------------------------------------------------------------- the server

def call(method, path, body=None, timeout=15):
    req = urllib.request.Request(BASE + path, method=method,
        data=None if body is None else json.dumps(body).encode(),
        headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        sys.exit(f"stopping: {method} {path} answered {e.code}: {e.read().decode()[:400]}")
    except (urllib.error.URLError, TimeoutError) as e:
        sys.exit(f"stopping: {method} {path} failed: {e}")

class Session:
    """Holds the specs and one context for a chart, and drops them on exit."""
    def __init__(self, chart, shapes, pause):
        self.pause, self.specs, self.ctx = pause, {}, None
        for s in shapes:
            self.specs[s] = call("POST", "/specs", spec_for(s))["spec_id"]
            time.sleep(pause)
        self.ctx = "ba55" + str(uuid.uuid4())[4:]
        t0 = time.perf_counter()
        put = call("PUT", f"/contexts/{self.ctx}", {"system": GUIDANCE_SYSTEM, "persist": False,
            "warm": list(self.specs.values()),
            "turns": [{"role": "user", "content": GUIDANCE, "snap": True},
                      {"role": "user", "content": chart_text(chart), "snap": True}]}, timeout=60)
        print(f"# context {self.ctx}: fed {put['fed']} tokens, warmed {len(self.specs)} spec(s) "
              f"in {(time.perf_counter() - t0) * 1000:.0f} ms", file=sys.stderr)
    def decide(self, shape, state, window):
        time.sleep(self.pause)
        body = {"spec_id": self.specs[shape], "contexts": [{"id": self.ctx}], "state": state, "timeout_ms": 10000}
        if ask_for(shape, window):
            body["ask"] = ask_for(shape, window)
        t0 = time.perf_counter()
        d = call("POST", "/decisions", body)
        wall = (time.perf_counter() - t0) * 1000
        return {"answers": {q: {"probabilities": a["probabilities"]} for q, a in d["answers"].items()},
                "mass": min(a.get("mass", 0) for a in d["reads"][0]["answers"].values()),
                "wall_ms": round(wall, 1), "server_ms": d.get("ms"), "usage": d.get("usage"),
                "control_text": d.get("signals", {}).get("control_text"), "identity": d.get("identity")}
    def close(self):
        if self.ctx:
            call("DELETE", f"/contexts/{self.ctx}")
        for sid in self.specs.values():
            call("DELETE", f"/specs/{sid}")

# ---------------------------------------------------------------- commands

def window_line(i, start, window, rec, picks, cells):
    c = coherence(cells)
    lat = f"{rec['wall_ms']:6.0f} ms" if rec.get("wall_ms") is not None else "replay"
    print(f"window {i + 1} bars {start + 1}-{start + window}: {lat} in {rec['usage']['input_tokens'] if rec.get('usage') else '-'} tok "
          f"mass {rec['mass']:.3f} | {' '.join(f'{k}={v}' for k, v in picks.items())} | "
          f"distinct {c['distinct']} changes {c['changes']} turnaround {c['turnaround']}", file=sys.stderr)

def play(args, chart):
    rng, bars, log, last = random.Random(args.seed), [], [], None
    s = Session(chart, [args.shape], args.pause)
    try:
        for i in range(args.windows):
            start = i * args.window
            state = case(chart, start, args.window, last)
            rec = s.decide(args.shape, state, args.window)
            rec.update(shape=args.shape, window=args.window, start=start, state=state)
            log.append(rec)
            cells, picks = cells_for(args.shape, args.window, rec["answers"], args, rng)
            window_line(i, start, args.window, rec, picks, cells)
            bars += render_window(chart, start, cells)
            last = cells[-1]
    finally:
        s.close()
    if args.log:
        Path(args.log).write_text("".join(json.dumps(r) + "\n" for r in log))
    print(abc(chart, bars))

def replay(args, chart):
    rng, bars = random.Random(args.seed), []
    for i, line in enumerate(l for l in Path(args.replay).read_text().splitlines() if l.strip()):
        rec = json.loads(line)
        cells, picks = cells_for(rec["shape"], rec["window"], rec["answers"], args, rng)
        window_line(i, rec["start"], rec["window"], dict(rec, wall_ms=None), picks, cells)
        bars += render_window(chart, rec["start"], cells)
    print(abc(chart, bars))

def compare(args, chart):
    shapes = ["per_bar", "form", "plan"]
    s, log = Session(chart, shapes, args.pause), []
    try:
        for shape in shapes:
            for window in (4, 8, 16):
                state = case(chart, 0, window, None)
                rec = s.decide(shape, state, window)
                rec.update(shape=shape, window=window, start=0, state=state)
                log.append(rec)
                print(f"{shape:8} {window:2} bars: {rec['wall_ms']:6.0f} ms wall, {rec['server_ms']:.0f} ms server, "
                      f"{rec['usage']['input_tokens']} input tokens, mass {rec['mass']:.3f}", file=sys.stderr)
    finally:
        s.close()
    if args.log:
        Path(args.log).write_text("".join(json.dumps(r) + "\n" for r in log))
    for rec in log:
        for policy, seeds in (("argmax", [0]), ("sample", range(1, 6))):
            for seed in seeds:
                a = argparse.Namespace(**{**vars(args), "policy": policy})
                cells, picks = cells_for(rec["shape"], rec["window"], rec["answers"], a, random.Random(seed))
                c = coherence(cells)
                print(f"{rec['shape']:8} {rec['window']:2} {policy:6} {seed} distinct {c['distinct']} changes {c['changes']:2} "
                      f"turn {'yes' if c['turnaround'] else 'no '} | {' '.join(cells)}")

if __name__ == "__main__":
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--chart", default=str(HERE / "charts" / "chameleon.json"),
                    help="chart JSON with key, mode, meter, changes, title (default charts/chameleon.json)")
    ap.add_argument("--key", help="tonic, like Bb, D or F#; overrides the chart")
    ap.add_argument("--mode", help="dorian, mixolydian, major, minor, ...; overrides the chart")
    ap.add_argument("--meter", help="only 4/4 is supported; overrides the chart")
    ap.add_argument("--changes", help='chords one per bar, repeating, like "Dm7 G7"; overrides the chart')
    ap.add_argument("--title", help=argparse.SUPPRESS)
    ap.add_argument("--shape", choices=["plan", "form", "per_bar"], default="plan", help="spec shape (default plan)")
    ap.add_argument("--window", type=int, choices=[4, 8, 16], default=8, help="bars per decision (default 8)")
    ap.add_argument("--windows", type=int, default=2, help="decisions to chain, at most 8 (default 2)")
    ap.add_argument("--seed", type=int, default=1, help="sampling seed (default 1)")
    ap.add_argument("--temperature", type=float, default=1.8,
                    help="above 1 flattens the distribution before sampling, below 1 sharpens it (default 1.8)")
    ap.add_argument("--policy", choices=["sample", "argmax"], default="sample",
                    help="sample the distribution, or take the top option (default sample)")
    ap.add_argument("--pause", type=float, default=1.0, help="seconds between requests (default 1.0)")
    ap.add_argument("--log", help="write each decision's distributions and timings as JSON lines")
    ap.add_argument("--replay", help="re-sample a --log file with no server calls")
    ap.add_argument("--compare", action="store_true", help="one decision per shape at 4, 8 and 16 bars")
    ap.add_argument("--print-spec", choices=["form", "per_bar", "plan"], help="print a shape's spec and exit")
    ap.add_argument("--render-all", action="store_true", help="print every cell on every chord as ABC, no server")
    a = ap.parse_args()
    if a.print_spec:
        print(json.dumps(spec_for(a.print_spec), indent=2)); sys.exit()
    ch = load_chart(a)
    if a.render_all:
        bars = []
        for i in range(len(ch["_chords"])):
            bars += [render(c, chord_at(ch, i), chord_at(ch, i + 1), ch["_key"]) for c in CELL_NAMES]
        print(abc(ch, bars)); sys.exit()
    if a.windows > 8:
        sys.exit("at most 8 windows per run: the server is shared")
    replay(a, ch) if a.replay else compare(a, ch) if a.compare else play(a, ch)
