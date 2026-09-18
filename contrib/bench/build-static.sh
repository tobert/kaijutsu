#!/usr/bin/env bash
# Build fully static x86_64 kaijutsu-solo-acp, kaijutsu-server and
# kaijutsu-acp binaries for arbitrary benchmark task containers, and prove
# portability. Runs entirely
# in a rootless podman container; the host and this worktree are never
# touched by cargo. All build output lives under WORK_ROOT (real disk), not
# /tmp.
#
# Usage: contrib/bench/build-static.sh
# Reads the worktree from WORKTREE (default: this script's repo root).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKTREE="${WORKTREE:-$(cd "${SCRIPT_DIR}/../.." && pwd)}"
WORK_ROOT="${WORK_ROOT:-/home/atobey/src/bench-work/dist}"

CARGO_HOME_DIR="${WORK_ROOT}/cargo-home"
TARGET_DIR="${WORK_ROOT}/target"
OUT_DIR="${WORK_ROOT}/out"
PKG_DIR="${WORK_ROOT}"

IMAGE="localhost/kaijutsu-static-builder:latest"

# kaijutsu-solo-acp is the one-command agent a benchmark launches; the server
# and bridge ride along for a two-process setup.
BINS=("kaijutsu-solo-acp" "kaijutsu-server" "kaijutsu-acp")
PACKAGE_ARGS=()
for b in "${BINS[@]}"; do
    PACKAGE_ARGS+=("--package" "${b}")
done

mkdir -p "${CARGO_HOME_DIR}" "${TARGET_DIR}" "${OUT_DIR}"

echo "==> [1/5] building the toolchain image (no source baked in)"
podman build --quiet \
    -f "${SCRIPT_DIR}/Containerfile.static" \
    -t "${IMAGE}" \
    "${SCRIPT_DIR}"

echo "==> [2/5] cargo build --release inside the container"
echo "    worktree (ro):   ${WORKTREE}"
echo "    CARGO_HOME:      ${CARGO_HOME_DIR}"
echo "    CARGO_TARGET_DIR:${TARGET_DIR}"
# --target is passed explicitly (even though host == x86_64-unknown-linux-musl
# on this image) so cargo classifies build scripts and proc-macro crates as
# HOST artifacts, separate from the TARGET artifacts. The static-link flags
# go in CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS, which only applies
# to target compilation — a blanket RUSTFLAGS applies to host builds too, and
# proc-macro crates cannot be built as +crt-static (rustc refuses: "does not
# support these crate types"). musl's crt-static is already the default here;
# -static/-static-libgcc/-static-libstdc++ additionally force the linker to
# statically resolve libgcc/libstdc++ (present as .a in this image via g++),
# since aws-lc-sys and other C/C++ build deps would otherwise pull in
# libgcc_s.so.1 / libstdc++.so.6 at runtime (see the root Containerfile's
# runtime-stage `apk add ... libgcc libstdc++`, which this build avoids
# needing).
STATIC_RUSTFLAGS="-C target-feature=+crt-static -C link-arg=-static -C link-arg=-static-libgcc -C link-arg=-static-libstdc++"
time podman run --rm \
    -v "${WORKTREE}:/src:ro" \
    -v "${CARGO_HOME_DIR}:/cargo-home" \
    -v "${TARGET_DIR}:/target" \
    -w /src \
    -e "CARGO_HOME=/cargo-home" \
    -e "CARGO_TARGET_DIR=/target" \
    -e "CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS=${STATIC_RUSTFLAGS}" \
    "${IMAGE}" \
    cargo build --release --locked \
        --target x86_64-unknown-linux-musl \
        "${PACKAGE_ARGS[@]}"

BIN_SRC="${TARGET_DIR}/x86_64-unknown-linux-musl/release"

echo "==> [3/5] copying + stripping binaries"
for b in "${BINS[@]}"; do
    cp -f "${BIN_SRC}/${b}" "${OUT_DIR}/${b}"
    chmod +w "${OUT_DIR}/${b}"
    strip "${OUT_DIR}/${b}"
done

echo "==> linkage evidence"
for b in "${BINS[@]}"; do
    echo "--- ${b} ---"
    file "${OUT_DIR}/${b}"
    ldd "${OUT_DIR}/${b}" 2>&1 || true
done

echo "==> [4/5] portability smoke tests (debian bookworm-slim, ubuntu 24.04, alpine 3.22)"
for img in "docker.io/library/debian:bookworm-slim" "docker.io/library/ubuntu:24.04" "docker.io/library/alpine:3.22"; do
    echo "--- ${img} ---"
    podman run --rm \
        -v "${OUT_DIR}:/dist:ro" \
        "${img}" \
        sh -c 'for b in "$@"; do "/dist/${b}" --help >"/tmp/${b}.out" 2>&1; echo "${b} exit=$?"; head -2 "/tmp/${b}.out"; done' sh "${BINS[@]}"
done

echo "==> [5/5] packaging"
TARBALL="${PKG_DIR}/kaijutsu-agent-linux-x86_64.tar.gz"
tar -C "${OUT_DIR}" -czf "${TARBALL}" "${BINS[@]}"
sha256sum "${TARBALL}"
ls -lh "${TARBALL}"

echo "==> done"
