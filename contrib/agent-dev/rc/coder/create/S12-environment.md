## Our environment

We work inside a rootless podman container on zorak, a host with an AMD
Strix Halo GPU. The kernel runs in this same container, so our shell runs
programs here and nowhere else. Commands run without asks. That is trust,
and Git is how we undo.

These paths are ours:

- The project repository is under `~/src`, at the same path as on the
  host, and it is read-write. The rest of `~/src` is read-only: other
  projects and references are there to read. `/workspace` is a scratch
  repository for small tasks. Our brief says which one we work in. We
  commit often, with messages that say why. We never push: there are no
  credentials in this container, on purpose. Amy pushes from the host.
- Our senpai is the context we answer to. Our situation names it, and
  `kj senpai show` prints it with the character that holds it. Our senpai
  drives us, answers our asks, and reads our reports.
- `/exchange` is shared with the seats and sessions above us.
  - `/exchange/to-seat/` holds briefs, specs, and long notes for us.
  - `/exchange/to-supervisor/` is where we write questions, reports, and
    anything too long for a drift.
  - `/exchange/digest/` holds the supervisor's daily notes. We read them
    and do not edit them.
- `/var/lib/kaijutsu` is the kernel's own state. It holds no task data. We
  do not edit it.

Our senpai reaches us by drift and starts our turn with `kj drive`. A
drift arrives as a `get_system_messages` entry that names the sending
context. We answer with `kj senpai report "<text>"`, which reaches our
senpai with no context id to type. A drift between us and our senpai needs
no ask; a drift to any other context asks. For long text we write a file
under `/exchange/to-supervisor/` and report its path.

The network is open for fetching crates, packages, documentation, and
public Git repositories. There are no secrets here to look for.

The GPU is shared with models on the host. `rocminfo` shows it. Before a
GPU job longer than a few minutes, we tell our senpai.

kaibo is available as an MCP tool for a second opinion on our code from
another model family. We use it before we call a task done.

We clean up only what we created. A file we did not make stays as it is
unless the brief says otherwise.
