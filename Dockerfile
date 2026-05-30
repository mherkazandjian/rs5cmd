# Rust build/dev environment for rs5cmd
FROM rust:1-bookworm

# Common tooling for dev + CI
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        pkg-config \
        libssl-dev \
        git \
    && rm -rf /var/lib/apt/lists/*

# Cache cargo registry across builds via a dedicated volume mount in compose
ENV CARGO_HOME=/usr/local/cargo
ENV CARGO_TARGET_DIR=/workspace/target

RUN rustup component add clippy rustfmt

WORKDIR /workspace

# Default to an interactive shell; compose overrides the command for test runs.
CMD ["bash"]
