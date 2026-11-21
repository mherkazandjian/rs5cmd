#!/usr/bin/env bash
# A3 baseline matrix, run from the host, one batch after the other (never two at once).
#
#   bench/review/a3.sh [batch...]        # default: every batch below except `uring`
#   bench/review/a3.sh uring             # the io_uring batch, inside fast-window.sh
#
# Results: results/$TAG.jsonl (main), results/$TAG-uring.jsonl (io_uring window); log:
# results/$TAG.log. TAG defaults to a3.
#
# Stage B re-baselines run the same batches on another build:
#   TAG=b0 RS5CMD_BIN=/opt/bins/b-stage TEST_SRC=/src/wt/b-stage bench/review/a3.sh [batch...]
# RS5CMD_BIN replaces the binary behind `rs5cmd` and `rs5cmd-fast`; TEST_SRC is the tree whose
# `--features fast` tests run in the io_uring window (default: this checkout).
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
D="$here/dc"
TAG="${TAG:-a3}"
LOG="${LOG:-$here/results/$TAG.log}"
TEST_SRC="${TEST_SRC:-/src/rs5cmd}"
PASS=()
[ -n "${RS5CMD_BIN:-}" ] && PASS=(-e "RS5CMD_BIN=$RS5CMD_BIN")
mkdir -p "$here/results"

# run [-e VAR=VAL ...] -- matrix args...
run() {
    local envs=()
    while [ "$1" != "--" ]; do envs+=("$1"); shift; done
    shift
    echo "== $(date -u +%FT%TZ) ${envs[*]} $*" >> "$LOG"
    "$D" exec -T "${PASS[@]}" "${envs[@]}" bench bash bench/review/run-matrix.sh run "$@" >> "$LOG" 2>&1
    rc=$?; echo "== $(date -u +%FT%TZ) exit=$rc" >> "$LOG"
}

MAIN="s5cmd,rs5cmd,rs5cmd-fast"   # rs5cmd-fast runs on its epoll fallback outside the window
W1='W1.up.100k,W1.dn.100k,W1.up.10k,W1.dn.10k,W1.up.1k,W1.dn.1k,W1.up.deep,W1.dn.deep,W1.up.10k.disk,W1.dn.10k.disk'
W2='W2.up,W2.dn,W2.up.disk,W2.dn.disk'
W3='W3.*'
REST='W4.*,W10.*,W5.up.*,W5.dn.*,W6.*,W7.*,W8.*,W9.*'
WORKERS='W1.up.100k.w*,W1.dn.100k.w*'
HEAD='W1.up.100k,W1.dn.100k,W3.up.4x1g,W3.dn.4x1g,W4.ls.1m'

b_stub()    { run -- --rows "$W1,$W2,$W3,$REST" --endpoint stub --clients "$MAIN" --tag "$TAG"; }
b_workers() { run -- --rows "$WORKERS" --endpoint stub --clients s5cmd,rs5cmd --tag "$TAG"; }
b_tls()     { run -- --rows "W1.up.100k,W1.dn.100k,W1.up.10k,W1.dn.10k,$W2,W3.up.4x1g,W3.dn.4x1g,W3.up.32x128m,W3.dn.32x128m,W3.up.1x4g,W3.dn.1x4g" \
                  --endpoint stub-tls --clients "$MAIN" --tag "$TAG"; }
b_minio()   { run -- --rows "W1.up.10k,W1.dn.10k,W1.up.100k,W1.dn.100k,W2.up,W2.dn,W3.up.4x1g,W3.dn.4x1g,W3.up.32x128m,W3.dn.32x128m,W4.ls.100k,W10.du.100k,W5.minio.*,W6.rm.10k,W7.*,W8.*,W9.*" \
                  --endpoint minio --clients "$MAIN" --tag "$TAG"; }
b_goversions() { run -- --rows "$HEAD" --endpoint stub --clients s5cmd,s5cmd-go122,s5cmd-golatest --tag "$TAG-go"; }
b_pinning() {
    run -e CPUSET_CLIENT=4-7,12-15 -- --rows "$HEAD" --endpoint stub --clients s5cmd,rs5cmd --tag "$TAG" --variant unpinned
    run -e CPUSET_CLIENT=6,7 -- --rows "$HEAD" --endpoint stub --clients s5cmd,rs5cmd --tag "$TAG" --variant 1tpc
}
b_pipe()    { run -- --rows 'W1.up.10k,W1.dn.10k,W4.ls.100k,W4.ls.1m,W6.rm.100k' --endpoint stub --clients s5cmd,rs5cmd --tag "$TAG" --stdout pipe; }
b_uring() {
    "$here/fast-window.sh" bash -c "
        for ep in stub stub-tls; do
            '$D' exec -T ${PASS[*]} bench bash bench/review/run-matrix.sh run --rows '$W1,$W2,$W3' --endpoint \$ep --clients rs5cmd-fast --tag '$TAG-uring'
        done
        '$D' exec -T ${PASS[*]} bench bash bench/review/run-matrix.sh run --rows 'W1.up.10k,W1.dn.10k,W1.up.100k,W1.dn.100k,W2.up,W2.dn,W3.up.4x1g,W3.dn.4x1g,W3.up.32x128m,W3.dn.32x128m' --endpoint minio --clients rs5cmd-fast --tag '$TAG-uring'
        # The --features fast test suite (compiled beforehand with --no-run), on io_uring.
        echo \"== \$(date -u +%FT%TZ) cargo test --all --features fast\"
        '$D' exec -T -e CARGO_TARGET_DIR=/target/test-b bench bash -c 'cd $TEST_SRC && cargo test --all --features fast --locked' > '$here/results/$TAG-fast-tests.log' 2>&1
        echo \"== \$(date -u +%FT%TZ) fast tests exit=\$?\"
    " >> "$LOG" 2>&1
}

# Stage A gaps (plan W10 and the sensitivity items), first run in the Wave 0 re-baseline.
LAT='W1.up.10k,W1.dn.10k,W3.up.4x1g,W3.dn.4x1g,W6.rm.10k,W7.*'
b_latency() { run -- --rows "$LAT" --endpoint stub --clients "$MAIN" --tag "$TAG" --latency-ms 20 --variant lat20; }
b_faults()  { run -- --rows 'W1.up.10k,W1.dn.10k' --endpoint stub --clients "$MAIN" --tag "$TAG" --fault-rate 0.01 --variant f503; }
b_sens() {
    local rows="$HEAD,W1.up.10k,W1.dn.10k,W6.rm.10k,W8.run.dn"
    run -e CPUSET_CLIENT=4-7,12-15 -- --rows "$rows" --endpoint stub --clients "$MAIN" --tag "$TAG" --variant unpinned
    run -e CPUSET_CLIENT=6,7 -- --rows "$rows" --endpoint stub --clients "$MAIN" --tag "$TAG" --variant 1tpc
}
# Stripping does not change s5cmd's speed: the local Go 1.22 build against its unstripped twin.
b_unstripped() {
    run -- --rows "$HEAD,W1.up.10k,W1.dn.10k" --endpoint stub --clients s5cmd,s5cmd-go122,s5cmd-go122-unstripped \
        --client-bin s5cmd-go122-unstripped=/opt/bins/s5cmd-go122-debug --tag "$TAG-go"
}
# glibc malloc thresholds for rs5cmd (no code change): a fixed mmap threshold of 128 KiB (large
# buffers come from mmap and go back on free) and a low trim threshold.
b_malloc() {
    local bin="${RS5CMD_BIN:-/opt/bins/rs5cmd}" cb=() c="s5cmd,rs5cmd"
    "$D" exec -T bench bash -c "
        printf '#!/bin/sh\nGLIBC_TUNABLES=glibc.malloc.mmap_threshold=131072 exec $bin \"\$@\"\n' > /opt/bins/rs5cmd-mmap128k
        printf '#!/bin/sh\nGLIBC_TUNABLES=glibc.malloc.trim_threshold=131072 exec $bin \"\$@\"\n' > /opt/bins/rs5cmd-trim128k
        chmod +x /opt/bins/rs5cmd-mmap128k /opt/bins/rs5cmd-trim128k"
    for v in mmap128k trim128k; do c+=",rs5cmd-$v"; cb+=(--client-bin "rs5cmd-$v=/opt/bins/rs5cmd-$v"); done
    run -- --rows 'W1.up.100k,W1.dn.100k,W3.dn.32x128m,W3.dn.4x1g,W5.up.warm,W4.ls.1m' --endpoint stub \
        --clients "$c" "${cb[@]}" --tag "$TAG" --variant malloc
}

batches=("$@")
[ ${#batches[@]} -eq 0 ] && batches=(stub workers tls minio goversions pinning pipe)
for b in "${batches[@]}"; do
    echo "#### batch $b start $(date -u +%FT%TZ)" >> "$LOG"
    "b_$b"
    echo "#### batch $b end $(date -u +%FT%TZ)" >> "$LOG"
done
