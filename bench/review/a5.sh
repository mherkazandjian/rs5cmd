#!/usr/bin/env bash
# A5 quick experiments, run from the host, one batch after the other (never two at once).
# Each experiment binary (build.sh exp-<name>, branch perf/exp-*) runs interleaved with s5cmd and
# the unchanged rs5cmd on the rows its change targets.
#
#   bench/review/a5.sh [batch...]     # default: every batch below
#
# Results: results/a5.jsonl (E8: results/a5-e8.jsonl); log: results/a5.log.
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
D="$here/dc"
LOG="${LOG:-$here/results/a5.log}"
mkdir -p "$here/results"

run() {
    echo "== $(date -u +%FT%TZ) $*" >> "$LOG"
    "$D" exec -T bench bash bench/review/run-matrix.sh run --tag a5 "$@" >> "$LOG" 2>&1
    rc=$?; echo "== $(date -u +%FT%TZ) exit=$rc" >> "$LOG"
}
exp() { # exp <name> -> --clients and --client-bin for s5cmd, rs5cmd, rs5cmd-<name>
    echo "--clients s5cmd,rs5cmd,rs5cmd-$1 --client-bin rs5cmd-$1=/opt/bins/exp-$1"
}

# E1: plain prefix match instead of a match-all regex for non-wildcard sources.
b_e1() { run --rows 'W4.ls.100k,W4.ls.1m,W10.du.1m,W1.dn.100k,W6.rm.100k' --endpoint stub $(exp e1); }
# E2: stream ranged-GET bodies to disk instead of collecting whole parts.
b_e2() {
    run --rows 'W3.dn.1x4g,W3.dn.4x1g,W3.dn.32x128m,W3.dn.4x256m.disk,W2.dn,W1.dn.10k' --endpoint stub $(exp e2)
    run --rows 'W3.dn.4x1g,W3.dn.32x128m,W2.dn' --endpoint minio $(exp e2)
    run --rows 'W3.dn.4x1g,W3.dn.32x128m' --endpoint stub-tls $(exp e2)
}
# E3: one buffered stdout writer instead of a locked, line-flushed print per line.
b_e3() {
    run --rows 'W4.ls.100k,W4.ls.1m,W1.dn.100k,W6.rm.100k' --endpoint stub $(exp e3)
    run --rows 'W4.ls.100k,W4.ls.1m,W1.dn.100k' --endpoint stub --stdout pipe $(exp e3)
}
# E4: up to 10 DeleteObjects batches in flight.
b_e4() {
    run --rows 'W6.rm.100k,W6.rm.10k' --endpoint stub $(exp e4)
    run --rows 'W6.rm.10k' --endpoint minio $(exp e4)
}
# E7: SdkConfig cached per (region, profile) for the whole process (run / sync / rm).
b_e7() {
    run --rows 'W8.run.dn,W8.run.up,W9.cp1,W5.up.warm' --endpoint stub $(exp e7)
    run --rows 'W8.run.dn,W8.run.up' --endpoint minio $(exp e7)
}
# E9: the SDK's default checksum behaviour (no WhenRequired overrides) - the cost of CRC32.
b_e9() { run --rows 'W1.up.10k,W1.dn.10k,W2.up,W2.dn,W3.up.4x1g,W3.dn.4x1g' --endpoint stub $(exp e9); }
# E10: read upload bodies from files in 256 KiB chunks instead of the SDK's 4 KiB.
b_e10() {
    run --rows 'W3.up.4x1g,W3.up.32x128m,W3.up.4x256m.disk,W2.up,W1.up.10k' --endpoint stub $(exp e10)
    run --rows 'W3.up.4x1g,W2.up' --endpoint stub-tls $(exp e10)
}
# E1 memory check: local sources (cp upload, sync) build one Url, and one compiled regex, per file.
b_e1mem() { run --rows 'W1.up.100k,W5.up.warm' --endpoint stub $(exp e1); }
# E1 on the rest of the rows that walk local files or build a Url per object (sync both ways).
b_e1sync() {
    run --rows 'W1.up.10k,W1.up.deep,W5.up.cold,W5.up.changed,W5.up.delete' --endpoint stub $(exp e1)
    run --rows 'W5.dn.cold,W5.dn.warm,W5.dn.changed,W5.dn.delete' --endpoint stub $(exp e1)
}
# E6: allocator and malloc tuning, via wrapper clients (bins/rs5cmd-<variant>).
b_e6() {
    local c="s5cmd,rs5cmd,rs5cmd-jemalloc,rs5cmd-mimalloc,rs5cmd-arena2,rs5cmd-thp" cb=()
    for v in jemalloc mimalloc arena2 thp; do cb+=(--client-bin "rs5cmd-$v=/opt/bins/rs5cmd-$v"); done
    run --rows 'W1.up.100k,W1.dn.100k,W3.dn.32x128m,W3.dn.4x1g,W5.up.warm,W4.ls.1m' --endpoint stub \
        --clients "$c" "${cb[@]}"
}
# E8: SDK floor - aws-sdk-s3 vs a lean hyper client with rs5cmd's own signer (bins/sdkfloor).
b_e8() {
    echo "== $(date -u +%FT%TZ) e8.py" >> "$LOG"
    "$D" exec -T bench python3 bench/review/e8.py --count 10000 --inflight 256 --rounds 10 >> "$LOG" 2>&1
    rc=$?; echo "== $(date -u +%FT%TZ) exit=$rc" >> "$LOG"
}
# Release-profile knobs (size.sh knobs): do they change speed? Small objects, large over TLS, ls.
b_knobs() {
    local c="s5cmd,rs5cmd" cb=()
    for k in fat-cgu1 fat-cgu1-os fat-cgu1-abort-os; do  # rs5cmd itself is the base
        c+=",rs5cmd-knob-$k"; cb+=(--client-bin "rs5cmd-knob-$k=/opt/bins/knob-$k")
    done
    run --rows 'W1.dn.10k,W1.up.10k,W4.ls.1m' --endpoint stub --clients "$c" "${cb[@]}"
    run --rows 'W3.dn.4x1g,W3.up.4x1g' --endpoint stub-tls --clients "$c" "${cb[@]}"
}

batches=("$@")
[ ${#batches[@]} -eq 0 ] && batches=(e1 e2 e3 e4 e7 e9 e8 e6 knobs)
for b in "${batches[@]}"; do
    echo "#### batch $b start $(date -u +%FT%TZ)" >> "$LOG"
    "b_$b"
    echo "#### batch $b end $(date -u +%FT%TZ)" >> "$LOG"
done
