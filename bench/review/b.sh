#!/usr/bin/env bash
# Stage B: before/after rounds for one change, interleaved with s5cmd. Run from the host, one
# batch at a time (never two at once).
#
#   bench/review/b.sh <change> <endpoint> <rows> [matrix args...]   # the rows the change targets
#   bench/review/b.sh <change> sentinel                              # broad rows, for regressions
#
# Clients: s5cmd; rs5cmd-before = /opt/bins/${BEFORE:-b-stage} (the perf/stage-b tip the change
# branched from); rs5cmd-<change> = /opt/bins/<stage>-<change>, <stage> = ${STAGE:-b} (Stage C:
# STAGE=c). With FAST=1 their `--fast` twins run too (on the epoll fallback unless the batch runs
# inside fast-window.sh).
# Results: results/<stage>-<change>.jsonl; log: results/b.log.
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
D="$here/dc"
LOG="${LOG:-$here/results/b.log}"
mkdir -p "$here/results"

change="$1"; shift
stage="${STAGE:-b}"
before="${BEFORE:-b-stage}"
clients="s5cmd,rs5cmd-before,rs5cmd-$change"
cb=(--client-bin "rs5cmd-before=/opt/bins/$before" --client-bin "rs5cmd-$change=/opt/bins/$stage-$change")
if [ "${FAST:-0}" = 1 ]; then
    clients+=",rs5cmd-before-fast,rs5cmd-$change-fast"
    cb+=(--client-bin "rs5cmd-before-fast=/opt/bins/$before" --client-bin "rs5cmd-$change-fast=/opt/bins/$stage-$change")
fi

run() {
    echo "== $(date -u +%FT%TZ) $change (before: $before) $*" >> "$LOG"
    "$D" exec -T bench bash bench/review/run-matrix.sh run --tag "$stage-$change" --clients "$clients" \
        "${cb[@]}" "$@" >> "$LOG" 2>&1
    rc=$?; echo "== $(date -u +%FT%TZ) exit=$rc" >> "$LOG"
}

# One row per family on the stub, short ones only, plus MinIO's small-object and rm rows: every
# change runs these too, to catch a regression outside the rows it targets.
SENTINEL_STUB='W1.up.10k,W1.dn.10k,W2.up,W2.dn,W3.up.4x1g,W3.dn.4x1g,W4.ls.100k,W10.du.100k,W5.up.warm,W5.dn.changed,W6.rm.10k,W7.cat,W8.run.dn,W9.ls1,W9.cp1'
SENTINEL_MINIO='W1.up.10k,W1.dn.10k,W6.rm.10k'

if [ "${1:-}" = sentinel ]; then
    run --rows "$SENTINEL_STUB" --endpoint stub
    run --rows "$SENTINEL_MINIO" --endpoint minio
else
    ep="$1" rows="$2"; shift 2
    run --rows "$rows" --endpoint "$ep" "$@"
fi
