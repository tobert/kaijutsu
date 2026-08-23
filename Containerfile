# Kaijutsu's runtime is intentionally small: the kernel, its protocol bridges,
# a POSIX shell for agent commands, and CA roots for provider HTTPS. Development
# tools belong in the devcontainer stage below, not in the kernel image.
FROM docker.io/library/rust:1.97-alpine3.22 AS build

RUN apk add --no-cache build-base capnproto-dev cmake git

WORKDIR /src
COPY . .

RUN cargo build --quiet --locked --release \
    --package kaijutsu-server \
    --package kaijutsu-mcp \
    --package kaijutsu-acp

FROM docker.io/library/alpine:3.22 AS runtime

# Alpine supplies BusyBox's /bin/sh. Keep that small execution baseline: the
# kernel's external-exec policy controls what a context may run, while a
# shell-less image would make ordinary kaish commands fail before that policy.
RUN apk add --no-cache ca-certificates libgcc libstdc++ \
    && addgroup --system --gid 10001 kaijutsu \
    && adduser --system --uid 10001 --ingroup kaijutsu --home /var/lib/kaijutsu kaijutsu \
    && install -d --owner kaijutsu --group kaijutsu --mode 0750 /var/lib/kaijutsu /workspace

COPY --from=build /src/target/release/kaijutsu-server /usr/local/bin/kaijutsu-server
COPY --from=build /src/target/release/kaijutsu-mcp /usr/local/bin/kaijutsu-mcp
COPY --from=build /src/target/release/kaijutsu-acp /usr/local/bin/kaijutsu-acp

# Name the configured production runtime. A final alias after the devcontainer
# target makes this the image `podman build .` publishes.
FROM runtime AS release

ENV HOME=/var/lib/kaijutsu \
    XDG_CACHE_HOME=/var/lib/kaijutsu/cache \
    XDG_CONFIG_HOME=/var/lib/kaijutsu/config \
    XDG_DATA_HOME=/var/lib/kaijutsu/data \
    XDG_RUNTIME_DIR=/tmp/kaijutsu-runtime

WORKDIR /workspace
USER kaijutsu
EXPOSE 2222

ENTRYPOINT ["/usr/local/bin/kaijutsu-server"]
CMD ["--port", "2222"]

# The development image deliberately inherits none of the runtime user's XDG
# environment. `vscode` owns source edits; start the server explicitly as
# `kaijutsu` so it writes only the persistent kernel-data mount.
FROM mcr.microsoft.com/devcontainers/rust:1-1-bookworm AS devcontainer

RUN groupadd --system --gid 10001 kaijutsu \
    && useradd --system --uid 10001 --gid kaijutsu --home-dir /var/lib/kaijutsu --create-home kaijutsu \
    && install -d --owner kaijutsu --group kaijutsu --mode 0750 /workspace

COPY --from=build /src/target/release/kaijutsu-server /usr/local/bin/kaijutsu-server
COPY --from=build /src/target/release/kaijutsu-mcp /usr/local/bin/kaijutsu-mcp
COPY --from=build /src/target/release/kaijutsu-acp /usr/local/bin/kaijutsu-acp

# The last stage is what `podman build .` publishes. Keep this alias after the
# devcontainer target so the documented default is always the production image.
FROM release AS production
