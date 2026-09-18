# Terminal-Bench 2.0 subset for a cheap model on one workstation

20 tasks out of the 89-task **`terminal-bench@2.0`** dataset, pinned to commit
`69671fbaac6d67a7ef0dfec016cc38a64ef7a77c`
(`https://github.com/laude-institute/terminal-bench-2.git`). Read from the
pinned clone at `/home/atobey/src/bench-work/harbor/repos/tb2/`
(`task.toml`, `environment/Dockerfile`, `environment/setup.sh`, and
`instruction.md` for each candidate — not guessed from names), per
`/home/atobey/src/bench-work/harbor/NOTES.md`, "4. Terminal-Bench 2.0".

Sized for DeepSeek flash on a single workstation under rootless podman:
stratified across 12 of the dataset's 16 categories, difficulty mix 3
easy / 12 medium / 5 hard, every base image `ubuntu:24.04` or a
`python:3.1x-slim` variant (no task here pulls a multi-GB base), every
`agent_timeout_sec` at or under 1800 (30 minutes), and every candidate's
Dockerfile/setup.sh checked for a GPU, a heavy ML install (torch,
tensorflow, caffe, mteb, fasttext, SAM, transformers), or a large runtime
download before inclusion.

## The subset

| Task | Difficulty | Category | agent_timeout_sec | Base image | Why chosen |
|---|---|---|---|---|---|
| fix-git | easy | software-engineering | 900 | python:3.13-slim-bookworm | Already verified end to end with `oracle` (NOTES.md §3); small repo, fast, deterministic `git` fix. |
| cobol-modernization | easy | software-engineering | 900 | python:3.13-slim-bookworm | Self-contained COBOL source file, no network, no heavy toolchain beyond what the image already has. |
| overfull-hbox | easy | debugging | 750 | ubuntu:24.04 | LaTeX compile-and-fix; fastest verify timeout in the whole dataset (360s), nothing to download. |
| fix-code-vulnerability | hard | security | 900 | python:3.11-slim | Fixes a CWE in a provided source file; no external service, no crypto brute force. |
| configure-git-webserver | hard | system-administration | 900 | ubuntu:24.04 | Local git+HTTP server config; everything runs inside the container, nothing fetched. |
| sparql-university | hard | data-querying | 900 | ubuntu:24.04 | Query over a provided local Turtle file; the dataset's only data-querying task, deterministic to verify. |
| model-extraction-relu-logits | hard | mathematics | 900 | python:3.13-slim-bookworm | `forward.py` is a tiny provided one-layer network queried locally — no model download, no GPU, despite the name. |
| dna-assembly | hard | scientific-computing | 1800 | ubuntu:24.04 | Sequence-file assembly from a provided `sequences.fasta`; no external database lookups in the instruction. |
| sqlite-with-gcov | medium | system-administration | 900 | ubuntu:24.04 | Instruction explicitly says to use the pre-vendored source tarball "instead of fetching sources over the network." |
| openssl-selfsigned-cert | medium | security | 900 | python:3.13-slim-bookworm | Local `openssl` cert generation, nothing external. |
| query-optimize | medium | data-science | 900 | ubuntu:24.04 | SQL rewrite over a small (WordNet-derived) sqlite file already baked into the image at build time. |
| regex-log | medium | data-processing | 900 | ubuntu:24.04 | Pure text/regex task over a provided log file. |
| db-wal-recovery | medium | file-operations | 900 | ubuntu:24.04 | SQLite WAL recovery on a provided local database file. |
| extract-elf | medium | file-operations | 900 | ubuntu:24.04 | ELF-format parsing of a provided binary; deterministic, fast to verify. |
| largest-eigenval | medium | mathematics | 900 | python:3.13-slim-bookworm | Numeric linear algebra on a provided `eigen.py` stub; numpy-scale, not GPU-scale. |
| constraints-scheduling | medium | personal-assistant | 1200 | ubuntu:24.04 | The dataset's only personal-assistant task; a scheduling constraint puzzle, no external calendar service. |
| chess-best-move | medium | games | 900 | ubuntu:24.04 | Reads a provided board image, decides a move; CPU chess search, no GPU. |
| headless-terminal | medium | software-engineering | 900 | python:3.13-slim-bookworm | Implements a documented local interface; no network. |
| modernize-scientific-stack | medium | scientific-computing | 600 | python:3.13-slim-bookworm | Shortest build timeout (600s) of any scientific-computing task in the set. |
| raman-fitting | medium | scientific-computing | 900 | python:3.13-slim-bookworm | Curve-fitting over a provided spectroscopy output file; scipy-scale numerics. |

Totals: 3 easy / 12 medium / 5 hard. Categories covered: software-engineering
(3), scientific-computing (3), security (2), system-administration (2),
mathematics (2), file-operations (2), debugging (1), data-querying (1),
data-science (1), data-processing (1), personal-assistant (1), games (1).

## Notable exclusions

- **Named-heavy-ML, confirmed by instruction/Dockerfile, not just by name**:
  `caffe-cifar-10` (installs and trains BVLC Caffe), `torch-pipeline-parallelism`
  and `torch-tensor-parallelism` (PyTorch required by the task itself),
  `pytorch-model-cli`, `pytorch-model-recovery`, `sam-cell-seg` (Facebook
  SAM/MobileSAM), `hf-model-inference` and `mteb-leaderboard`/`mteb-retrieve`
  (`transformers`/`mteb` in the Dockerfile), `train-fasttext`, `gpt2-codegolf`
  (downloads GPT-2 checkpoint weights).
- **VM/OS images**: `install-windows-3.11`, `qemu-alpine-ssh`, `qemu-startup`
  (QEMU + `.iso` boot images — heavy and slow under nested/rootless podman).
- **Runtime network download of the task's own input**: `extract-moves-from-video`
  (downloads a YouTube video at run time — an external dependency this
  harness should not need on every run).
- **Over the ~30-minute agent budget** (`agent_timeout_sec` > 1800):
  `build-pov-ray` (12000s), `compile-compcert` (2400s), `bn-fit-modify`,
  `distribution-search`, `mteb-leaderboard`, `mteb-retrieve`, `portfolio-optimization`,
  `reshard-c4-data`, `sam-cell-seg`, `train-fasttext`, and others at 3600s+.
  `portfolio-optimization` is the dataset's only `optimization`-category
  task, so that category has no representative here for this reason alone.
- **Statistical-toolchain compiles flagged for caution, not included**:
  `mcmc-sampling-stan` and `rstan-to-pystan` (Stan model compilation is a
  real, sometimes slow, native build step) — left out in favor of the
  simpler numeric tasks (`largest-eigenval`, `raman-fitting`,
  `modernize-scientific-stack`) that cover the same categories more cheaply.
- **Large external dataset by name, not fully verified light**: `reshard-c4-data`
  (the C4 corpus is normally huge; excluded rather than assume the task's
  provided sample is small) and `count-dataset-tokens` (pulls a Hugging
  Face dataset at run time) — both left out even though their Dockerfiles
  looked lean, to keep every task's network dependency to "files already in
  the image."
- **`video-processing`**: installs `opencv-contrib-python` and needs a
  provided MP4; kept out of the fixed 20 to leave the `video-processing`
  and `machine-learning`/`model-training` categories entirely off this cheap
  run rather than including one questionable representative of each.

## Running it

```bash
cd /home/atobey/src/bench-work/harbor
source env.sh

INCLUDE_FLAGS=()
while IFS= read -r task; do
  INCLUDE_FLAGS+=(-i "$task")
done < <(grep -v '^#' /home/atobey/src/wt/kaijutsu-bench/contrib/bench/analysis/tb2-subset.txt)

harbor run --dataset "terminal-bench@2.0" "${INCLUDE_FLAGS[@]}" \
  --agent terminus-2 --model "deepseek/deepseek-chat" \
  --ae "DEEPSEEK_API_KEY=$DEEPSEEK_API_KEY" \
  -e podman -k 1 -n 2 \
  -o /home/atobey/src/bench-work/harbor/jobs --job-name "tb2-subset-deepseek" -y
```

`-e podman` (the environment this workstation has working, per NOTES.md
§2), `-k 1` (one attempt per task, no retries — this is a cheap-model
survey, not a pass@k measurement), `-n 2` (two trials at a time: podman
compose builds each task's image from scratch, and this is one
workstation, not a cluster — raise it only after watching one run's CPU/
memory headroom). `--agent terminus-2 --model deepseek/deepseek-chat` is
the DeepSeek-routed agent shape read (not yet run) in NOTES.md §7; it
needs `DEEPSEEK_API_KEY` exported in the shell before this runs. This is
LLM spend — do not run it without asking first.

Summarize the resulting job with this repo's own tool:

```bash
python3 /home/atobey/src/wt/kaijutsu-bench/contrib/bench/analysis/summarize_job.py \
  /home/atobey/src/bench-work/harbor/jobs/tb2-subset-deepseek
```
