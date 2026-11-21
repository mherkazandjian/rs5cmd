#!/usr/bin/env bash
# A4 profiling, run from the host after A3/A5 (never during a measurement batch). Each step is one
# profile.sh mode on a few rows (1 round, no warm-up); outputs go to
# bench/review/prof/<mode>/ (git-ignored). Plus one fault-injection run (F04).
#
#   bench/review/a4.sh [step...]      # default: every step below
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
D="$here/dc"
LOG="${LOG:-$here/results/a4.log}"
mkdir -p "$here/results"

# Stage B: PROF_OUT=<dir in the container> and RS5CMD_BIN=<build> profile another build without
# overwriting Stage A's profiles, e.g. LOG=.../b0-a4.log PROF_OUT=/src/rs5cmd/bench/review/
# prof-b0 RS5CMD_BIN=/opt/bins/b-stage bench/review/a4.sh sched smaps
PASS=()
[ -n "${PROF_OUT:-}" ] && PASS+=(-e "PROF_OUT=$PROF_OUT")
# PROF_TAG (e.g. prof-w1) names the results files, so Stage A's results/prof-*.jsonl stay as they are.
[ -n "${PROF_TAG:-}" ] && PASS+=(-e "PROF_TAG=$PROF_TAG")
[ -n "${RS5CMD_BIN:-}" ] && PASS+=(-e "RS5CMD_BIN=$RS5CMD_BIN")
prof() { # prof <mode> <rows> <endpoint> <clients> [extra]
    echo "== $(date -u +%FT%TZ) profile $*" >> "$LOG"
    "$D" exec -T "${PASS[@]}" bench bash bench/review/profile.sh "$@" >> "$LOG" 2>&1
    rc=$?; echo "== $(date -u +%FT%TZ) exit=$rc" >> "$LOG"
}

SMALL='W1.dn.10k,W1.up.10k,W2.dn,W4.ls.100k,W6.rm.10k,W8.run.dn'
LARGE='W3.dn.4x1g,W3.up.4x1g,W3.dn.32x128m'
SYNC='W5.up.warm'

s_fp() {
    prof fp "$SMALL,$LARGE,$SYNC" stub rs5cmd-prof,s5cmd-prof
    prof fp 'W1.dn.10k,W1.up.10k,W3.dn.4x1g' stub rs5cmd-prof-fast
    prof fp 'W1.dn.10k,W3.dn.4x1g' stub-tls rs5cmd-prof,s5cmd-prof
}
s_dwarf()  { prof dwarf 'W1.dn.10k,W3.dn.4x1g' stub rs5cmd-prof; }
s_stat()   { prof stat "$SMALL,$LARGE" stub rs5cmd,s5cmd,rs5cmd-fast; }
s_strace() { prof strace "$SMALL,$LARGE" stub rs5cmd,s5cmd,rs5cmd-fast; }
s_heap()   { prof heap 'W1.up.100k,W1.dn.100k,W3.dn.32x128m,W5.up.warm' stub rs5cmd-prof; }
s_smaps()  { prof smaps 'W1.up.100k,W1.dn.100k,W3.dn.32x128m,W5.up.warm' stub rs5cmd,s5cmd,rs5cmd-fast; }
s_sched()  { prof sched "$SMALL,$LARGE,$SYNC" stub rs5cmd,s5cmd,rs5cmd-fast; }
# F04: under faults (503 SlowDown on 30 % of object requests), how long until a multipart transfer
# gives up, and how many requests does it still send after the first part has failed for good?
s_fault() {
    echo "== $(date -u +%FT%TZ) fault" >> "$LOG"
    "$D" exec -T bench bash bench/review/run-matrix.sh run --rows 'W3.up.4x1g,W3.dn.4x1g' \
        --endpoint stub --clients s5cmd,rs5cmd --rounds 2 --warmup 0 --fault-rate 0.3 \
        --tag a4-fault >> "$LOG" 2>&1
    rc=$?; echo "== $(date -u +%FT%TZ) exit=$rc" >> "$LOG"
}

steps=("$@")
[ ${#steps[@]} -eq 0 ] && steps=(fp dwarf stat strace heap smaps fault)
for s in "${steps[@]}"; do
    echo "#### step $s start $(date -u +%FT%TZ)" >> "$LOG"
    "s_$s"
    echo "#### step $s end $(date -u +%FT%TZ)" >> "$LOG"
done
