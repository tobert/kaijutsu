# Council dossier: decision APIs outside kaijutsu

Source material for `docs/council.md` and `docs/council-api.md`. It records what
other decision APIs look like, as of 2026-10-04, so the council API's choices
can be checked against them. Every claim names its source. Most sources are
secondary (blogs, gateway docs, press); none of them is a vendor specification
we have read in full. Re-check before relying on a detail.

## The shape in one example

The decision APIs below share one request shape: application **state** plus a
map of typed **questions**, answered with probabilities instead of text.

```json
POST /api/alpha/decisions
{"model": "typesafe/jev-1.13",
 "state": "Been using this for three weeks and the scheduler silently dropped two posts.",
 "questions": {
   "intent": {"type": "choice",
              "instructions": "What does this reply want from the account owner",
              "criteria": {"bug_report": "Describes something that is broken or behaved unexpectedly",
                           "other": "Anything else"}}}}
```

```json
{"model": "typesafe/jev-1.13",
 "answers": {"intent": {"type": "choice", "choice": "bug_report",
                        "probabilities": {"bug_report": 0.93, "other": 0.07},
                        "confidence": 0.88}},
 "usage": {"input_tokens": 61, "output_tokens": 0}}
```

The request fields are from the Portkey and OpenRouter references and the Jev
examples below. The response numbers are illustrative, not a recorded reply.

## Jev (TypeSafe AI)

- **What it is.** A hosted "System One" model that answers typed questions and
  writes no text. Released 2026-09-15 by TypeSafe AI (founder Diogo Almeida,
  ex-OpenAI; $40M raised, per InfoQ). Model ids `jev-latest`, `jev-preview`,
  `jev-1.13.0`. No open weights are mentioned anywhere we read.
- **Direct endpoint.** `POST https://api.typesafe.ai/v1/systemone` with
  `state`, `model`, `questions` (a search summary of TypeSafe's docs, not read
  directly; flaviocopes gives the base URL `https://api.typesafe.ai`).
  `state` is a string, a JSON object, or an array.
- **Question types.** Each question has `type`, `instructions`, and optional
  `criteria`. Question ids are the caller's and are never shown to the model.
  - `choice`: one of up to 255 labels, `criteria` maps label to description.
    The Node guide says "always include an `other` label."
  - `score`: an ordered scale of 2 to 10 levels. Answers `score` (the
    probability-weighted level number, so it can fall between levels),
    `legend` (level number to description), `probabilities` keyed by level
    number, and `confidence`.
  - `noul`: a yes/no statement. Answers one number from 0 to 1, with no
    `confidence`. The name is TypeSafe's own coinage.
- **Answers pick a winner.** A `choice` answer names the chosen label beside
  the distribution. What `confidence` measures is not documented anywhere we
  read.
- **One pass.** All questions are evaluated "in a single parallel pass"
  (InfoQ). There is no describe step and no reasoning trace.
- **Numbers.** 32,000-token context. $0.042 per million input tokens, output
  free. Claimed 70–500 ms end to end. TypeSafe's "jaggedness" page warns of
  unreliable counting, arithmetic and date comparison, and accuracy loss on
  large noisy state (InfoQ).
- **Caching.** Nothing documents a held question set or a cached state
  prefix. The questions are resent with every request.
- **Client behavior** (TypeScript SDK, flaviocopes): 10 s default timeout, two
  retries with backoff from 500 ms to 5 s, honors `Retry-After` up to 60 s.
- **Adoption.** An arXiv survey counted 2,170 repositories using Jev, 1,865 of
  them created in the week after release.

## The Decisions API family (gateways)

Jev's request shape has become a shared gateway API within three weeks:

- **OpenRouter** serves `POST https://openrouter.ai/api/alpha/decisions` (also
  `/api/v1/systemone`). Its docs say "every decision model on OpenRouter
  speaks the same Decisions API" with the three primitives `choice`, `noul`,
  `score`. The Go SDK's `DecisionsRequest` has `model`, `state`, `questions`
  (map of id to question), and optional `provider`, `session_id` (up to 256
  characters, for grouping related requests), `trace`, and `user`. Responses
  carry `usage.cost` in USD. Model ids `typesafe/jev-1.13`,
  `~typesafe/jev-latest`.
- **Portkey** documents `POST /decisions`: question `type` is `noul`, `choice`
  or `score`; `instructions` is a string, object or array; `criteria` is an
  object, an array, or null. Answers are `{type, noul}`, `{type, choice,
  probabilities, confidence}` and `{type, score, probabilities, legend,
  confidence}`, under `answers` keyed by question id, with optional `usage` and
  `provider`.
- **Eden AI** lists a decisions endpoint too; we did not read it.

Inferred, not verified: a `score` question's levels travel in `criteria` as an
array. The Node SDK's `score(instructions, [levels])` and Portkey's
"object | array" type both suggest it.

## OpenAI Decisions API

- Announced at DevDay, 2026-09-29, running on GPT-6 Luna. Described as a
  "fast decision layer" for classification, routing, tool-call gates, and an
  agent's next step (Hugging Face blog; eesel).
- Limited preview. On 2026-10-02 an ordinary key got
  `{"error": {"message": "Decision API is not enabled for this user.",
  "type": "invalid_request_error"}}` from `POST /v1/decisions` (eesel). That
  path comes from the 403, not from OpenAI documentation.
- Publicly described inputs: context as text or images, and questions with a
  finite list of allowed answers. Nothing is published about confidences,
  several questions per call, caching, limits, or pricing.
- OpenAI staff claim "less than a few hundred milliseconds end to end"; the
  eesel author's approximation with Luna structured outputs measured a 1.46 s
  median. That approximation is not the Decisions API.
- Whether OpenAI's request shape matches the gateway family is unknown.

## What our two servers do that these do not

From `~/src/megakernel-qwen38-flashnext-strixhalo/docs/developer-guide.md`
(the `/mk/v1` API) and `~/src/lfm2d/docs/integration.md`, items 17–19, and
`~/src/lfm2d/docs/lfm25-adjudicator.md`, "The opinion API":

- **Held state.** Content-addressed held contexts that later requests extend
  or read after; the megakernel parks them to disk so they outlive a restart.
- **Held question sets.** lfm2d's specs are loaded once and read from a menu;
  a request names a spec and may only narrow its options. lfm2d measured why:
  framing words in a request move label tokens by an order of magnitude.
- **Describe first.** lfm2d's spec orders text fields before the verdict, and
  the model writes them before the read. On LFM2.5 a verdict read before the
  description carried nothing (AUC 0.46–0.55) and one after it was the
  judgment (AUC 0.74).
- **No-opinion signal.** Both report the probability mass the model put on
  the answers at all (`mass`, `sequence_mass`). Low mass means the model was
  not answering this question, not that it gave a low score.
- **No winner.** Neither server names a chosen answer; the caller applies its
  own thresholds per question set and refits when the model identity changes.
- **Several contexts, pooled.** One case read after 1 to 8 held contexts, with
  linear or loglinear pooling, `agree`, `spread`, and (lfm2d) leave-one-out.
- **Bit-identical replay.** The same identity and inputs give the same bits.

## Sources

Read 2026-10-04.

- [InfoQ: TypeSafe AI releases Jev](https://www.infoq.com/news/2026/10/typesafe-ai-jev-released/)
- [How to use Jev in Node.js (flaviocopes)](https://flaviocopes.com/jev-nodejs.md)
- [Jev on OpenRouter](https://openrouter.ai/docs/guides/community/jev)
- [OpenRouter Go SDK: DecisionsRequest](https://openrouter.ai/docs/client-sdks/go/models/components/decisionsrequest.md)
- [Portkey: Decisions API reference](https://portkey.ai/docs/api-reference/inference-api/decisions.md)
- [Eden AI: create decisions](https://www.edenai.co/docs/api-reference/decisions/create-decisions.md)
- [Jev in the Wild (arXiv 2609.30216)](https://arxiv.org/html/2609.30216v1)
- [OpenAI Decisions API explained (eesel)](https://www.eesel.ai/blog/openai-decisions-api)
- [What Is OpenAI Decisions API? (Hugging Face blog)](https://huggingface.co/blog/sora-2/what-is-openai-decisions-api-a-practical-guide)
