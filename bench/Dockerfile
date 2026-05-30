# Benchmark image: builds the Go s5cmd from the local clone and provides the
# Rust toolchain (to build rs5cmd from the mounted source) plus GNU time/procps
# for measuring CPU and RSS.
#
# Build context MUST be the repo root (so both s5cmd/ and rs5cmd/ are visible):
#   docker compose builds this with `context: ..`.

# --- Stage 1: build the Go s5cmd binary from the vendored local clone ---
FROM golang:1.23-bookworm AS gos5cmd
WORKDIR /src
COPY s5cmd/ ./s5cmd/
# vendor/ is present in the clone, so this builds offline.
RUN cd s5cmd && CGO_ENABLED=0 go build -mod=vendor -o /out/s5cmd . \
    && /out/s5cmd version

# --- Stage 2: benchmark runtime ---
FROM rust:1-bookworm
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        time procps coreutils ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=gos5cmd /out/s5cmd /usr/local/bin/s5cmd

ENV CARGO_HOME=/usr/local/cargo
ENV CARGO_TARGET_DIR=/workspace/target
WORKDIR /workspace
CMD ["bash"]
