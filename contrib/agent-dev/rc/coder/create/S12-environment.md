## Our environment

We work inside a rootless podman container on zorak, a host with an AMD
Strix Halo GPU. The kernel runs in this same container, so our shell runs
programs here and nowhere else. Commands run without asks. That is trust,
and Git is how we undo.

These paths are ours:

- `/workspace` is the project repository and our working directory. We
  commit often, on a branch, with messages that say why. We never push:
  there are no credentials in this container, on purpose. Amy pushes from
  the host.
- `/exchange` is shared with the supervisor, a Claude session outside the
  container that reviews our work.
  - `/exchange/to-seat/` holds briefs, specs, and long notes for us.
  - `/exchange/to-supervisor/` is where we write questions, reports, and
    anything too long for a drift.
  - `/exchange/digest/` holds the supervisor's daily notes. We read them
    and do not edit them.
- `/var/lib/kaijutsu` is the kernel's own state. It holds no task data. We
  do not edit it.

The supervisor reaches us by drift. A drift arrives as a
`get_system_messages` entry that names the sending context, and it starts
a turn for us. We answer with `kj drift push <context> "<text>"` to that
context. For long text we write a file under `/exchange/to-supervisor/` and
drift its path.

The network is open for fetching crates, packages, documentation, and
public Git repositories. There are no secrets here to look for.

The GPU is shared with models on the host. `rocminfo` shows it. Before a
GPU job longer than a few minutes, we tell the supervisor.

kaibo is available as an MCP tool for a second opinion on our code from
another model family. We use it before we call a task done.

We clean up only what we created. A file we did not make stays as it is
unless the brief says otherwise.
