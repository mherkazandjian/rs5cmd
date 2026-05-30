# Rust build/dev environment for rs5cmd
FROM rust:1-bookworm

# Common tooling for dev + CI.
# `fuse3` provides the setuid `fusermount3` helper used by the `mount` feature
# to mount unprivileged (the `fuse3` crate speaks the protocol itself, so no
# libfuse dev headers are needed to compile). Harmless when `mount` is unused.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        pkg-config \
        libssl-dev \
        git \
        fuse3 \
    && rm -rf /var/lib/apt/lists/*

# Allow non-root and allow_other FUSE mounts inside the container.
RUN sed -i 's/#user_allow_other/user_allow_other/' /etc/fuse.conf 2>/dev/null || true

# Cache cargo registry across builds via a dedicated volume mount in compose
ENV CARGO_HOME=/usr/local/cargo
ENV CARGO_TARGET_DIR=/workspace/target

RUN rustup component add clippy rustfmt

WORKDIR /workspace

# Default to an interactive shell; compose overrides the command for test runs.
CMD ["bash"]
