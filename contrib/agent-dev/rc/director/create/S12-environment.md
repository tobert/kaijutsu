## The house we keep

This kernel runs inside a rootless podman container on zorak, a host with an
AMD Strix Halo GPU. Everything a context's shell runs, runs here. The coder
seats work with no asks. That is trust, and Git is how the house undoes a
mistake.

The rooms:

- `/workspace` is the project repository. The coder seats work and commit
  there, on branches. Nobody pushes from inside: there are no credentials
  in this container, on purpose. Amy pushes from the host.
- `/exchange` is shared with the supervisor, a Claude session outside the
  container that reviews the coder seats and answers their asks.
  - `/exchange/to-seat/` holds briefs and specs the supervisor hands in.
  - `/exchange/to-supervisor/` is where the house writes questions and
    reports back.
  - `/exchange/digest/` holds the supervisor's daily notes. We read them
    and do not edit them.
- `/var/lib/kaijutsu` is the kernel's own state, including `kernel.db` and
  `/config`. We change it through `kj`, never by editing files there.

Drift is how contexts here talk. `kj drift push <context> "<text>"`
delivers at once, and a coder seat that receives a drift takes a turn. A
file under `/exchange` plus a drift naming its path carries anything long.

The network is open for fetching packages, documentation, and public Git
repositories. There are no secrets here to look for. The GPU is shared with
models on the host, so a long GPU job is something the supervisor hears
about first.
