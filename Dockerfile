# syntax=docker/dockerfile:1.7
# Multi-stage build for Memento RS (T-105 in batch 11).
#
# Stage 1 (`build`): rust:1.95-bookworm produces the three release
# binaries — `memento`, `memento-mcp`, `memento-worker` — with
# stripped symbols and `lto = "thin"`. The toolchain tag matches
# `rust-toolchain.toml` (edition 2024 floor, rustc 1.95).
#
# Stage 2 (`runtime`): debian:bookworm-slim carries the binaries plus
# the runtime shared libs (libssl, libgcc, ca-certificates). No Node,
# no Cargo, no build tools — the image is the smallest surface that
# actually serves requests.

FROM rust:1.95-bookworm AS build

WORKDIR /src

# Layer-cached: copy only the manifests first so dependency resolution
# is invalidated only when the pins change.
COPY Cargo.toml Cargo.lock ./
COPY crates crates

# Release build with thin LTO + strip + codegen-units=1. `-j` honors the
# container's CPU quota; the host `scripts/bench.sh` uses `-j 2` only
# because w64devkit's ld runs out of memory on parallel links — glibc
# ld has no such constraint.
RUN cargo build --release --workspace --bins \
    && strip target/release/memento \
    && strip target/release/memento-mcp \
    && strip target/release/memento-worker

FROM debian:bookworm-slim AS runtime

# Runtime libs only: libssl3 (rmcp + aes-gcm deps transitively), libgcc
# (pulled by various C deps in lancedb/ort), ca-certificates (outbound
# HTTPS for the embed-model first-run download — `MultilingualE5Small`).
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        libssl3 \
        libgcc-s1 \
        ca-certificates \
        tini \
    && rm -rf /var/lib/apt/lists/*

# Non-root user; the worker / MCP server / CLI all run as `memento`.
RUN groupadd --system --gid 1000 memento \
    && useradd --system --uid 1000 --gid memento --home /var/lib/memento memento

WORKDIR /var/lib/memento

# Copy the stripped binaries from the build stage.
COPY --from=build /src/target/release/memento          /usr/local/bin/memento
COPY --from=build /src/target/release/memento-mcp      /usr/local/bin/memento-mcp
COPY --from=build /src/target/release/memento-worker   /usr/local/bin/memento-worker

# Persistent data dirs: tenant store, model cache, audit logs, backups.
# `docker-compose.yml` mounts named volumes on these.
RUN mkdir -p db models logs backups tmp \
    && chown -R memento:memento /var/lib/memento

USER memento

# Default to the CLI; `docker-compose.yml` overrides the command per service.
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["memento", "--help"]

# `memento health` is the liveness probe (REQ-OP-001 Q3).
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD memento health || exit 1
