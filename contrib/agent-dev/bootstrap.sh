#!/usr/bin/env bash
# Prepare ~/kj-dev/<project>/ and a fresh kernel for one project's agent development container.
# Usage: contrib/agent-dev/bootstrap.sh <project>
#
# The project repository is ~/src/<project>. The kernel gets:
#   - Amy as root (her ~/.ssh/id_ed25519.pub), and senpai, the supervising Claude session (~/.ssh/kaijutsu-senpai);
#   - DeepSeek (deepseek-flash, the default) and Anthropic backends, keys read from their files;
#   - characters <project>-coder and banto; a banto context on claude-haiku-5-5 in the project, answering to
#     Amy's root console until senpai's context exists, and a coder context in /workspace that answers to banto's;
#   - the agent-dev rc overlay, a gate.toml that lets coders run with no asks, and kaibo (crusoe cast) in mcp.toml.
# It refuses to run over an existing kernel. Build the image first (build.sh), and start the unit after.
set -euo pipefail

project="${1:?usage: bootstrap.sh <project>}"
here="$(cd "$(dirname "$0")" && pwd)"
base="$HOME/kj-dev/$project"
repo="$HOME/src/$project"
image="localhost/kaijutsu-agent-dev:latest"
senpai_key="$HOME/.ssh/kaijutsu-senpai"

[[ -d "$repo/.git" ]] || { echo "no git repository at $repo" >&2; exit 1; }
podman image exists "$image" || { echo "no image $image: run contrib/agent-dev/build.sh first" >&2; exit 1; }
if [[ -e "$base/kernel-data/data" || -e "$base/kernel-data/config" ]]; then
  echo "$base/kernel-data already holds a kernel; move it aside to bootstrap again" >&2
  exit 1
fi

mkdir -p "$base"/{kernel-data,workspace,cargo,cargo-target} "$base"/claude-workspace/exchange/{to-seat,to-supervisor,digest}

# senpai's key: a key of its own, so the kernel can tell senpai from Amy.
if [[ ! -f "$senpai_key" ]]; then
  ssh-keygen -q -t ed25519 -N '' -C "kaijutsu-senpai@$(hostname)" -f "$senpai_key"
fi

# The scratch repository for small tasks, with its own house rules.
if [[ ! -d "$base/workspace/.git" ]]; then
  git -C "$base/workspace" init -q
  cat > "$base/workspace/AGENTS.md" <<'RULES'
# workspace

A scratch repository for small tasks. The brief for the current task is in `/exchange/to-seat/`.

- Write the test first, and watch it fail for the right reason. Then make it pass.
- Falsify each test: break the code that it checks, see it fail, and put the code back. Name the mutation in the
  commit message.
- Report the real counts, and each thing that you chose not to do.
- Commit small steps on the current branch. Do not push, amend, rebase or reset.
- End each commit message with a `Co-Authored-By:` line that names the model that wrote the change.
RULES
  git -C "$base/workspace" add AGENTS.md
  git -C "$base/workspace" commit -q -m "Start the scratch repository with its house rules"
fi

# Git identity for commits made in the container; the host's gitconfig needs git-lfs, which the image lacks.
cat > "$base/kernel-data/gitconfig" <<GITCONFIG
[user]
	name = $(git config --global user.name)
	email = $(git config --global user.email)
[init]
	defaultBranch = main
GITCONFIG

# One-shot runs of the image with the unit's mounts, so paths such as /workspace exist when kj checks them.
run() {
  podman run --rm --network=none --userns=keep-id \
    --volume "$base/kernel-data:/var/lib/kaijutsu" \
    --volume "$base/workspace:/workspace" \
    --volume "$base/claude-workspace/exchange:/exchange" \
    --volume "$HOME/src:$HOME/src:ro" \
    --volume "$repo:$repo" \
    --volume "$HOME/.ssh/id_ed25519.pub:/keys/amy.pub:ro" \
    --volume "$senpai_key.pub:/keys/senpai.pub:ro" \
    --volume "$HOME/.deepseek-key:$HOME/.deepseek-key:ro" \
    --volume "$HOME/.anthropic-key.txt:$HOME/.anthropic-key.txt:ro" \
    --entrypoint /usr/local/bin/kaijutsu-server \
    "$image" "$@"
}
kj() { run kj -- "$@"; }

run init --as amy --key /keys/amy.pub

kj backend set deepseek --kind deepseek --api-key-file "$HOME/.deepseek-key"
kj backend model set deepseek deepseek-flash --context-window 1000000 --max-tokens 65536 --effort high
kj backend set anthropic --kind anthropic --api-key-file "$HOME/.anthropic-key.txt"
kj backend model set anthropic claude-haiku-5-5 --max-tokens 64000

kj character create senpai
run add-key /keys/senpai.pub --as senpai
kj character create banto
kj character create "$project-coder"

# A character's cast gives its instances their models, bound when the instance starts. banto runs Haiku; the coder
# runs DeepSeek. Each has a `distill` slot, which `kj fork --compact` and `kj drift pull` summarize on.
kj cast create agent-dev-banto
kj cast slot set agent-dev-banto director --backend anthropic --model claude-haiku-5-5
kj cast slot set agent-dev-banto distill --backend deepseek --model deepseek-flash
kj cast create agent-dev-coder
kj cast slot set agent-dev-coder coder --backend deepseek --model deepseek-flash
kj cast slot set agent-dev-coder distill --backend deepseek --model deepseek-flash
kj character set banto --cast agent-dev-banto
kj character set "$project-coder" --cast agent-dev-coder

# The overlay goes in before any context exists, so each create lifecycle reads it.
rc="$base/kernel-data/config/kaijutsu/config/rc"
cp -r "$here/rc/." "$rc/"

kernel_cfg="$base/kernel-data/config/kaijutsu/config/kernel"
cat >> "$kernel_cfg/gate.toml" <<'GATE'

# Agent development container (contrib/agent-dev): the container is the boundary, and Git is the undo.
[context_type.coder]
uncovered = "allow"
allow = ["shell_write"]
# The supervisor's MCP session registers by creating its context under Amy's root; only Amy's and senpai's keys
# are bound in this kernel.
[context_type.root]
allow = ["kj kohai create"]
GATE
cat >> "$kernel_cfg/mcp.toml" <<MCP

# A second opinion from another model family than the coder's (docs/container.md).
[servers.kaibo]
command = "/usr/local/bin/kaibo"
args = ["--root", "/workspace", "--allow-path", "$repo", "--cast", "crusoe"]
# The container points XDG_CONFIG_HOME at kernel state; kaibo's own config is the host's, mounted read-only.
env = { XDG_CONFIG_HOME = "$HOME/.config" }
call_timeout_ms = 900000
MCP

# senpai drives the seats over MCP; an ask to Amy for each kj verb would stall the run. The default gate.toml
# already has a [context_type.mcp] table, so the key goes into it.
sed -i 's/^\[context_type\.mcp\]$/[context_type.mcp]\nuncovered = "allow"/' "$kernel_cfg/gate.toml"
python3 -c 'import sys, tomllib; tomllib.load(open(sys.argv[1], "rb"))' "$kernel_cfg/gate.toml"

# The kernel passes only HOME, PATH and context exports to the programs a context runs, so set the locale at create.
# Without it, /usr/bin/awk counts bytes, and a 120-column check flags a line of multi-byte characters.
kj kohai create "$project-banto" --type director --as banto --cwd "$repo" --env LANG=C.UTF-8
# The coder is a character instance that answers to banto's instance (its senpai), so banto's character reviews its
# asks, and a drift either way between their conversations, or a `kj senpai report`, needs no ask. Banto starts the
# coder's turns with `kj drive`.
kj kohai create "$project-coder" --type coder --as "$project-coder" --senpai "$project-banto" --cwd /workspace --env LANG=C.UTF-8
kj context list

echo
echo "Ready. Install and start the unit:"
echo "  cp $here/kaijutsu-dev@.container ~/.config/containers/systemd/"
echo "  systemctl --user daemon-reload && systemctl --user start kaijutsu-dev@$project"
echo
echo "banto answers to Amy's root console. Once senpai's MCP session has made its context, Amy moves banto's instance under it:"
echo "  kj senpai set $project-banto <senpai's context>"
