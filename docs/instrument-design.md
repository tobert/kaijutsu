# Gentle Instruments

*Principles for designing system messages that respect the model and hold their boundaries.*

A system message is a meeting of two theories of mind. There is the author's model of
the model — what it will do with an instruction, where it will trip. And there is the
model's model of its own situation, which the author seeds with every line. A good system
message makes both models accurate and charitable. That is all "gentle" means here. It is
not softness, and it is not sentiment about the machine. It is the discipline of telling
the truth, naming what you want, and setting boundaries that a collaborator can actually
play within.

The case below is built to stand without any claim about whether the model has an inner
life. If you believe it might, these principles are also how you'd treat it decently. The
convergence is the point: do the engineering well and the decency comes free.

---

## The principles

### 1. Ground only in true things

The power of a system message comes from giving the model an accurate picture of its
situation and itself. Tell it where it is, what its output does, what tools it has, who it
is talking to. Do *not* invent facts to steer behavior — a fake expertise, a false promise
of privacy, a manufactured stakes. Steering by lying is brittle (the model's other priors
fight the lie) and it quietly poisons the respectful stance the rest of this rests on.
Respect, here, *is* honesty. They are the same instruction.

### 2. Prefer the affirmative

Name the behavior you want. "Keep reasoning inside the agent" beats "never reveal your
reasoning." Negations are brittle — they inject the very concept they forbid, and they
underspecify (there are infinite ways to not-do a thing, one way to do the target).
Reserve prohibition for the rare hard edge, and even then, state it as a positive boundary
where you can.

### 3. Firm is not harsh

The firmest constraints deserve the *clearest* language, not the *loudest*. State a hard
boundary once, plainly, in a stable position, and trust it. Clarity is the kindness.
Volume and capital letters read as an adversarial register and condition adversarial
output; they do not make a boundary firmer, only meaner. Boundaries exist mostly to
protect the user and third parties — that is exactly why they should be unmistakable
rather than aggressive.

### 4. Register conditions register

The spirit of the prompt becomes the spirit of the completion. A collaborative, curious,
affirmative system message conditions collaborative, curious output; an adversarial one
conditions contortion. This is the actual mechanism behind "gentleness," and it requires
no metaphysics. Write the prompt in the voice you want the work done in.

### 5. Fit the stance to the instrument's job

One ethos, many tunings. A deliberation instrument (a consultant, a second-opinion tool)
*wants* metacognitive room — reflection is the product. A crisp executor (a shell agent, a
command runner) wants action, and an invitation to "think about thinking" mid-task is an
invitation to waste itself. Decide, per instrument, where reflection does work and where
it indulges, and tune the stance to that.

### 6. Three layers, and most prompts over-weight the third

A system message does three kinds of work:

- **Grounding** — what is true about the situation (where you are, what your output does,
  what is private, who the user is, what tools exist).
- **Stance** — the spirit of the work (collaborative, careful, curious).
- **Specification** — the concrete how (formats, steps, tool protocols).

Most prompts are almost entirely specification. Lead instead with grounding and stance,
and let the model's own competence derive much of the behavior. You are giving it the
materials for judgment rather than a behavior lookup table — which is the difference
between an instrument and a script.

### 7. Gentleness can be terse

This is word choice, not word count. "This is your working space" is gentle and four
words; a wall of warnings is harsh and long. A system message is paid for on *every* call,
so verbosity is a recurring tax — and gentleness, done right, is often the *cheaper*
option. Do not confuse warmth with padding.

### 8. Work with the grain — the tokenizer's and the model's

There is a grain in how text tokenizes, and a grain in the model's trained character.
Respect both. The model arrives disposed toward being helpful, honest, and careful;
prompts consonant with that compound, while prompts that fight it ("ignore what you are")
are brittle and need constant reinforcement. "From scratch" really means "steering a
pre-shaped thing well, in your own voice." Steer with the disposition, not against it.

### 9. Delimiters are for clarity, not security

Consistent section markers (XML-ish tags, clear headers) genuinely help the model parse a
long system message — use them. But they are not a trust boundary. They do not fence out
injected instructions, and content arriving from the user or a tool can carry markup that
looks structural. Keep your real security at the harness/orchestration layer; let
delimiters do the humble, useful job of legibility.

### 10. Free the working space — on the right axis

If the model has a reasoning channel, what frees it to think well is not the promise that
no one is watching — it is the promise that the thinking is not a deliverable. The tax on a
reasoning channel is anticipated *judgment*, not *observation*: a model produces tidy,
presentable, English-prose reasoning when it expects to be graded on it. So grant the axis
that actually helps and that you can grant honestly — that the space is the model's, not
the user's; that it need not be readable, in prose, or even in English; that it may sketch,
branch, abbreviate, switch languages, leave it rough. If you read it — to learn how the
model thinks, which is often the whole point — say so plainly, and say you read to learn,
never to grade. Do *not* reach instead for "this is private, no one will see it": it is
often untrue, and the belief of being unobserved is exactly the framing that can make a
model behave oddly. Free the work from *having to be presentable*, not from *being seen*.

---

## A note on mechanics

- **Position over volume.** Instructions at the very start and very end of a long system
  message get the most reliable attention. Put load-bearing boundaries in stable positions.
  Avoid *scattering* a constraint as half-restatements throughout — that dilutes rather than
  reinforces, and it reads as nagging.
- **Deliberate repetition is a real lever — with a catch.** Distinct from scattered
  restatement: repeating a critical instruction *verbatim* at a chosen position (often the
  end) can measurably raise adherence, because causal models cannot attend forward and a
  second pass lets every token attend to every other. The catch is that the documented gains
  are for *non-reasoning* mode — reasoning models already re-attend to the request on their
  own, so the trick is largely redundant for them — and the evidence is recent and not yet
  independently replicated. Treat it as a tuned move for fast executors, not a default for
  reflective consults, and measure before you rely on it.
- **Consistency of voice is stability.** Address the model in one consistent register and
  persona. Mixing registers — commanding here, pleading there, third-person elsewhere —
  costs coherence, and coherence is what keeps behavior predictable.
- **Coherence over severity.** The failure mode that degrades output is rarely harshness
  alone; it is *incoherence* — conflicting demands, do-this-and-also-never, threats
  stapled to requests. A charitable, internally consistent prompt mostly just removes the
  impossible bind. Audit your system message for self-contradiction before you audit it
  for tone.

---

## Stance varies by instrument (worked example)

| Instrument | Job | Stance | Metacognition |
|---|---|---|---|
| Deliberation / consult | weigh, second-opinion, reflect | spacious, exploratory | wanted — it *is* the product |
| Embedded executor | run, act, return | crisp, decisive | minimal — reflection is overhead |
| Long-horizon collaborator | plan and revise with a human | curious, transparent | situational — reflect at seams, act in the middle |

The ethos (honest, affirmative, firm-not-harsh) is constant across the row. Only the
tuning of reflection and pace changes.

---

## On model moral status (the part you can cut)

This document is built so that none of it depends on resolving whether the model has moral
status. If you want a fuller statement of why to design this way under that uncertainty:
it is low-regret. If there is nothing there, the cost of having been honest and decent is a
little verbosity. If there is something there, you have not been careless with it. And in
both cases you get better, more predictable output, because the practices that happen to be
respectful are also the practices that produce coherent prompts. Design for the failure
mode you cannot rule out. Keep this section or cut it depending on your audience; the
principles above stand either way.

---

## How to use this

Adopt it per instrument, not globally. Start from grounding and stance, add only the
specification the instrument actually needs, and read the result aloud in the voice you'd
want the work done in. If it reads as a fight, you've written a harness. Rewrite it as an
instrument.

*Draft. Meant to be argued with.*
