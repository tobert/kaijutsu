# Control arm: Harbor's own agents, not kaijutsu

`run-control.sh` runs Harbor's pre-integrated `mini-swe-agent` (or
`terminus-2`) on the DeepSeek model kaijutsu runs by default, as a control
arm alongside `run-harbor.sh`'s `kaijutsu-solo-acp` runs. Same interface:
`--job-name`, `--task-path` | `--dataset`, `--task`, `--task-file`, `--`
passthrough.

```bash
./run-control.sh --job-name kj-control-hello \
  --task-path /home/atobey/src/harbor/examples/tasks/hello-world

./run-control.sh --job-name kj-control-polyglot-rust \
  --task-path /home/atobey/src/bench-work/harbor/datasets/polyglot-rust \
  --task-file ../analysis/polyglot-rust-tasks.txt
```

## The model string

`deepseek/deepseek-v4-flash` — litellm's `provider/model` form. `deepseek` is
a first-class litellm provider (`PROVIDERS["deepseek"]` in Harbor's
`agents/model_connection.py`: `DEEPSEEK_API_KEY`, base URL
`https://api.deepseek.com`); `deepseek-v4-flash` is the exact model id
kaijutsu's own deepseek backend sends on the wire
(`crates/kaijutsu-kernel/src/llm/deepseek/mod.rs`), so this is the same model
both arms compare, not a stand-in. **Observed**, real run: litellm already
knows this model id — `result.json`'s `agent_result.cost_usd` populated
correctly (see below), so no `litellm_model_registry`/`model_info` override
was needed.

## Limits and fairness

Ours: effort max, 32768-token output ceiling, 100-iteration cap per turn.
mini-swe-agent's own defaults, **read** from the installed package
(`minisweagent/config/mini.yaml` and `agents/default.py`, mini-swe-agent
2.4.6) and Harbor's wrapper (`agents/installed/mini_swe_agent.py`):

- **Step limit: unlimited.** `mini.yaml` ships `agent.step_limit: 0`, and the
  agent's own gate is `0 < step_limit <= n_calls` — only checked when
  `step_limit > 0`. Harbor's `MiniSweAgentOptions` exposes no CLI flag for it
  at all, so it never gets touched by a normal `harbor run`.
- **Cost limit: unlimited, and Harbor sets this explicitly.** `mini.yaml`
  ships `agent.cost_limit: 3.` (three dollars), but
  `MiniSweAgentOptions.cost_limit` defaults to the *string* `"0"`, and Harbor
  always renders a non-`None` option as a CLI flag — so a plain `harbor run
  -a mini-swe-agent` passes `--cost-limit 0` on every invocation, overriding
  the $3 YAML default with "no limit" before the agent ever sees it.
- **Reasoning effort / max tokens: off unless asked.** Neither field has a
  Harbor CLI default; left unset, mini-swe-agent gets no
  `reasoning_effort`/`max_tokens` override in its model kwargs at all.

This script does not silently narrow any of that to match ours. Set the
matching env var to opt in:

| Our setting | Env var | Mechanism |
|---|---|---|
| 100-iteration cap | `HARBOR_CONTROL_STEP_LIMIT` | mini-swe-agent: writes `agent: {step_limit: N}` to a small YAML, passed via `--ak config_file=`, merged on top of the packaged `mini` config (mini-swe-agent's own `-c` is a spec list, not a config; the module always prepends `-c mini` first). terminus-2: `--ak max_turns=N` directly. |
| effort max | `HARBOR_CONTROL_REASONING_EFFORT` | `--ak reasoning_effort=`. **Caveat**: `mini.yaml` sets `model.model_kwargs.drop_params: true`, so if DeepSeek's chat-completions API does not recognize `reasoning_effort`, litellm drops it silently rather than erroring — this knob may be a no-op for this model. Not verified live (see below). |
| 32768-token ceiling | `HARBOR_CONTROL_MAX_TOKENS` | `--ak max_tokens=` (mini-swe-agent only; terminus-2 has no equivalent kwarg, and the script refuses rather than silently ignoring it). |
| (cost cap) | `HARBOR_CONTROL_COST_LIMIT` | `--ak cost_limit=` (mini-swe-agent only; same refusal for terminus-2). |

Left unset, none of these appear on the command line — the agent's own
default above stands, unchanged.

## terminus-2 support

`HARBOR_CONTROL_AGENT=terminus-2` switches the agent and model flags; its own
docs (`docs/content/docs/agents/terminus-2.mdx`) show `max_turns` (default
1,000,000 — effectively unlimited, same shape of gap as mini-swe-agent's
step limit) and a `reasoning_effort` kwarg with a `"max"` value, which this
script maps `HARBOR_CONTROL_STEP_LIMIT`/`HARBOR_CONTROL_REASONING_EFFORT`
onto. This is **read only** — terminus-2 runs in its own Python process
outside the container (not a `BaseInstalledAgent`), and was not exercised in
this lane's one real run (budget was spent proving mini-swe-agent).

## Key hygiene, banner, and provenance

Same mechanism as `run-harbor.sh`: the key is read from
`HARBOR_CONTROL_KEY_FILE` (default `~/.deepseek-key`) into this shell's
environment and handed to Harbor as the template
`${DEEPSEEK_API_KEY}`, never as a literal; `set -x` is refused; the job
directory is scanned for the key after the run with the same grep
exit-status handling (0 = leak, exit 3; 1 = clean; other = unverified, exit
5); `PODMAN_COMPOSE_WARNING_LOGS=false` works around the same `podman
compose` banner. See `README.md`, "The provider key" and "Podman: the
compose banner" for the full detail — not repeated here.

`<job>/kaijutsu-control-job-provenance.json` adds one thing run-harbor.sh's
provenance does not need: `observed_agent_info`, read back from every
trial's own `result.json.agent_info` after the run, rather than assumed from
`HARBOR_CONTROL_AGENT` — because unlike kaijutsu (the only agent
`run-harbor.sh` ever launches), Harbor is resolving and versioning a
third-party agent here, and that resolution is exactly the kind of thing
that should be observed, not trusted.

## Verification run (real, hello-world)

Two real runs, both `mini-swe-agent` / `deepseek/deepseek-v4-flash`, `-e
podman -k 1 -n 1`: one by hand to prove the model string before the script
existed, one through the finished `run-control.sh`. Both: reward 1.0, key
scan clean, total spend well under a cent.

| | tokens in | tokens out | cache | cost_usd |
|---|---|---|---|---|
| hand-run | 12267 | 2043 | 10240 | 0.00312 |
| via `run-control.sh` | 3670 | 397 | 0 | 0.00064 |

`result.json`'s token/cost fields, unlike kaijutsu's (which are always
`null` — kaijutsu sends no `PromptResponse.usage`):

```
<trial>/result.json
  .agent_info.name            "mini-swe-agent"
  .agent_info.version         "2.4.6"
  .agent_info.model_info      {"name": "deepseek-v4-flash", "provider": "deepseek"}
  .agent_result.n_input_tokens
  .agent_result.n_output_tokens
  .agent_result.n_cache_tokens
  .agent_result.cost_usd
  .agent_result.model_usage."deepseek/deepseek-v4-flash"   # same four fields, per-model
```

`python3 ../analysis/summarize_job.py <job dir>` (run from the docs
worktree) already reads these correctly with **no changes needed**:
`summarize_trial()` falls back to `agent_result.n_input_tokens` /
`n_output_tokens` / `n_cache_tokens` / `cost_usd` whenever a trial has no
`agent/acp-events.jsonl` (true for any non-ACP agent), and tags the row
`tokens_source: "agent_result"`. Observed output for the `run-control.sh`
run: `tokens_in=3670 tokens_out=397 cost_usd=0.000637482
tokens_source=agent_result`, totals `pass_rate=1.0`.

## Sharing code with run-harbor.sh

Both scripts duplicate the same key-read/template/scan skeleton (roughly the
first and last thirds of each file) and the arg-parsing block
(`--job-name`/`--task-path`/`--dataset`/`--task`/`--task-file`/`--`). Worth
factoring into a sourced `lib.sh` if a third runner ever joins them; left
alone here since this lane's territory is new files only and
`run-harbor.sh` is out of scope to edit.
