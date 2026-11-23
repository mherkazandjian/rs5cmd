#!/usr/bin/env bash
# CPU and memory profiling for the review (A4). Runs inside the pinned bench container and
# reuses matrix.py's rows (setup, checks) with a profiler wrapped around the measured command.
#
#   bash bench/review/profile.sh <mode> <rows> <endpoint> <clients> [extra matrix.py args]
#
# Modes:
#   fp      perf record, frame-pointer call graphs, 1999 Hz -> folded stacks, flamegraph SVG,
#           self time by crate / leaf function
#   dwarf   perf record, DWARF unwinding (cross-check of fp), 199 Hz, 16 KiB stacks -> same outputs
#   stat    perf stat: cycles, instructions, task-clock, context switches, migrations, faults
#   strace  strace -f -c -w: syscall counts and wall time spent in each
#   heap    heaptrack (glibc-linked binaries only) -> peak heap consumers
#   smaps   smaps.py: RSS breakdown at the peak (heap / anon / stacks / file / sockets)
#   sched   schedstat.py: on-CPU time, run-queue wait, off-CPU time and timeslices per run
#
# Clients with symbols: rs5cmd-prof (rs5cmd, [profile.profiling] + frame pointers),
# rs5cmd-prof-fast (same binary with --fast), s5cmd-prof (s5cmd built with Go 1.22, unstripped).
# Outputs (not committed): bench/review/prof/<mode>/<row>.<client>.*
set -euo pipefail
RS=/src/rs5cmd
HERE="$RS/bench/review"
OUT="${PROF_OUT:-$RS/bench/review/prof}"
mode="$1" rows="$2" ep="$3" clients="$4"
shift 4
D="$OUT/$mode"
mkdir -p "$D"

M=(python3 -u "$HERE/matrix.py" run --rows "$rows" --endpoint "$ep" --clients "$clients"
   --rounds 1 --warmup 0 --tag "${PROF_TAG:-prof}-$mode"
   --client-bin rs5cmd-prof=/opt/bins/rs5cmd-prof
   --client-bin rs5cmd-prof-fast=/opt/bins/rs5cmd-prof
   --client-bin s5cmd-prof=/opt/bins/s5cmd-go122-debug)

case "$mode" in
    fp)     "${M[@]}" --wrap "perf record -q --no-buildid-cache -F 1999 -g --call-graph fp -o $D/{row}.{endpoint}.{client}.data --" "$@" ;;
    dwarf)  "${M[@]}" --wrap "perf record -q --no-buildid-cache -F 199 --call-graph dwarf,16384 -o $D/{row}.{endpoint}.{client}.data --" "$@" ;;
    stat)   "${M[@]}" --wrap "perf stat -x , -e cycles,instructions,task-clock,context-switches,cpu-migrations,page-faults -o $D/{row}.{endpoint}.{client}.stat --" "$@" ;;
    strace) "${M[@]}" --wrap "strace -f -c -w -o $D/{row}.{endpoint}.{client}.strace" "$@" ;;
    # heaptrack captures go to the tmpfs (they can be large); only the printed reports land in $D.
    heap)   mkdir -p /mnt/tmpfs/heap
            "${M[@]}" --wrap "heaptrack -o /mnt/tmpfs/heap/{row}.{endpoint}.{client}" "$@" ;;
    smaps)  "${M[@]}" --wrap "python3 $HERE/smaps.py $D/{row}.{endpoint}.{client}.json --" "$@" ;;
    sched)  "${M[@]}" --wrap "python3 $HERE/schedstat.py $D/{row}.{endpoint}.{client}.json --" "$@" ;;
    *) echo "unknown mode $mode" >&2; exit 2 ;;
esac

summarize_folded() { # <folded> -> <summary.txt>: self time by crate and by leaf function
    python3 - "$1" > "$2" <<'PY'
import collections, re, sys
leaf, crate, total = collections.Counter(), collections.Counter(), 0
incl = collections.Counter()
# Categories match where a frame's own function path starts (type parameters in generic names
# mention many crates, so a plain substring match over-counts). INCL: a sample counts if any
# frame matches (the time spent below that layer). SELF: only the leaf frame is tested.
P = r"^<?"
INCL = {
    "SDK orchestration (aws-smithy, aws-sdk-*, aws-sdk-go)": P + r"(aws_smithy_\w+|aws_sdk_\w+|aws_runtime|aws_config)::|^github\.com/aws/aws-sdk-go",
    "HTTP (hyper, h2, http, net/http)": P + r"(hyper|hyper_util|h2|httparse|http|monoio_http)::|^net/http",
    "SigV4 signing": P + r"(aws_sigv4|hmac)::|^<?rs5cmd::fastpath::sign|aws-sdk-go/aws/signer",
    "hashing (sha2, md5)": P + r"(sha2|md5|digest)::|^crypto/(md5|sha256)|sha256_block|md5_block",
    "TLS (rustls, aws-lc, ring, crypto/tls)": P + r"(rustls|tokio_rustls|monoio_rustls|aws_lc_rs|ring)::|^aws_lc_|^crypto/tls|aes_hw|aesni|gcm_",
    "XML": P + r"(xmlparser|aws_smithy_xml|quick_xml)::|^encoding/xml",
    "regex": P + r"(regex|regex_automata|regex_syntax|aho_corasick)::|^regexp",
    "output formatting / stdout": P + r"(core::fmt|alloc::fmt|std::io::stdio)|^fmt\.|^<?rs5cmd::output",
    "URL / key handling": P + r"rs5cmd::storage::url|^path/filepath|^<?url::",
    "file I/O (std::fs, tokio::fs, os.File)": P + r"(std::fs|tokio::fs|std::os::unix::fs)|^os\.\(\*File\)",
}
SELF = {
    "kernel": r"_\[k\]$",
    "allocator (malloc/free, Go GC)": r"^(__libc_)?(malloc|free|realloc|calloc|cfree)|^_int_(malloc|free)|malloc_consolidate|^tcache|^runtime\.(mallocgc|gcBgMarkWorker|scanobject|gcDrain|sweepone|bgsweep|greyobject|findObject)",
    "memcpy / memmove / memset": r"mem(cpy|move|set)",
    "async runtime (tokio) / Go scheduler": P + r"tokio::|^runtime\.(schedule|findRunnable|park_m|futex|netpoll|stealWork|mcall|lock|unlock|runqgrab|notewakeup|notesleep)",
}
ci = {k: re.compile(v) for k, v in INCL.items()}
cs = {k: re.compile(v) for k, v in SELF.items()}
incl, selfc = collections.Counter(), collections.Counter()
for line in open(sys.argv[1]):
    stack, _, n = line.rstrip().rpartition(" ")
    if not n.isdigit():
        continue
    n = int(n)
    total += n
    frames = stack.split(";")
    f = frames[-1]
    leaf[f] += n
    if f.endswith("_[k]"):
        c = "[kernel]"
    else:
        m = re.match(r"^(?:<)?([A-Za-z_][\w]*)(?:::|\.)", f)
        c = m.group(1) if m else f.split("(")[0][:40]
        if "/" in f.split("(")[0]:
            c = f.split(".")[0][:60]
    crate[c] += n
    for k, rx in ci.items():
        if any(rx.search(fr) for fr in frames):
            incl[k] += n
    for k, rx in cs.items():
        if rx.search(f):
            selfc[k] += n
            break
print(f"total samples {total}")
print("\n## inclusive share by layer (time in or below it; a sample can count in several)")
for k, v in incl.most_common():
    print(f"{100*v/total:6.1f}%  {k}")
print("\n## self time by kind (leaf frame)")
for k, v in selfc.most_common():
    print(f"{100*v/total:6.1f}%  {k}")
print("\n## self time by crate / package")
for k, v in crate.most_common(30):
    print(f"{100*v/total:6.1f}%  {k}")
print("\n## self time by leaf function")
for k, v in leaf.most_common(40):
    print(f"{100*v/total:6.1f}%  {k[:150]}")
PY
}

case "$mode" in
    fp|dwarf)
        for f in "$D"/*.data; do
            [ -e "$f" ] || continue
            b="${f%.data}"
            if [ -s "$b.folded" ] && [ "$b.folded" -nt "$f" ]; then
                [ "${KEEP_DATA:-0}" = 1 ] || rm -f "$f"
                continue
            fi
            perf script -i "$f" 2>/dev/null | inferno-collapse-perf --all > "$b.folded"
            inferno-flamegraph --title "$(basename "$b") ($mode)" < "$b.folded" > "$b.svg"
            summarize_folded "$b.folded" "$b.summary.txt"
            # The raw samples are large and sit on the host disk; the folded stacks keep them.
            [ "${KEEP_DATA:-0}" = 1 ] || rm -f "$f"
        done
        ;;
    heap)
        for f in /mnt/tmpfs/heap/*.zst /mnt/tmpfs/heap/*.gz; do
            [ -e "$f" ] || continue
            b="$D/$(basename "${f%.*}")"
            heaptrack_print -f "$f" -p 1 -a 0 -T 0 -l 0 -n 25 > "$b.heap.txt" 2>/dev/null || true
            [ "${KEEP_DATA:-0}" = 1 ] || rm -f "$f"
        done
        ;;
esac
if [ -n "${HOST_UID:-}" ] && [ "$HOST_UID" != 0 ]; then
    chown -R "$HOST_UID:$HOST_GID" "$OUT" "$HERE/results" 2>/dev/null || true
fi
echo "profile outputs: $D" >&2
