# Toolchain image for the optimization review (bench/review/).
#
# Installs only. Nothing is compiled in a RUN step: image builds run under BuildKit, which
# ignores the runtime cpuset and memory limits. Everything that compiles (cargo, go, cargo
# install of the helper tools) runs later in the pinned `bench` container. See README.md.
ARG RUST_IMAGE=rust:1-bookworm
ARG GO_OLD_IMAGE=golang:1.22-bookworm
ARG GO_NEW_IMAGE=golang:1-bookworm

FROM ${GO_OLD_IMAGE} AS go_old
FROM ${GO_NEW_IMAGE} AS go_new

FROM ${RUST_IMAGE}
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates curl xz-utils git file jq bc openssl zstd pigz \
        python3 time procps util-linux coreutils binutils \
        strace linux-perf heaptrack \
        libjemalloc2 libmimalloc2.0 \
    && rm -rf /var/lib/apt/lists/*

# Go toolchains for s5cmd: 1.22 as in upstream's release workflow, plus the latest Go.
COPY --from=go_old /usr/local/go /opt/go1.22
COPY --from=go_new /usr/local/go /opt/go-latest

# zig 0.13 for cargo-zigbuild, as in rs5cmd's release workflow (prebuilt tarball).
ARG ZIG_VERSION=0.13.0
RUN curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-linux-x86_64-${ZIG_VERSION}.tar.xz" \
        | tar -xJ -C /opt \
    && ln -s "/opt/zig-linux-x86_64-${ZIG_VERSION}/zig" /usr/local/bin/zig

# cargo-zigbuild, cargo-bloat and inferno are installed by `size.sh tools` into the
# `tools` volume (mounted at /opt/tools), from inside the pinned container.
ENV PATH=/opt/tools/bin:${PATH} \
    CARGO_HOME=/usr/local/cargo
WORKDIR /src/rs5cmd
CMD ["sleep", "infinity"]
