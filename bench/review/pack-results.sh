#!/usr/bin/env bash
# Compress finished raw results before committing them: every results/*.jsonl, and any *.log or
# *.out over 256 KiB, with `gzip -9 -n` (no name or time stamp, so the same input gives the same
# bytes). A file that is already packed (x.jsonl.gz) gets the newer plain records appended as one
# more gzip member; gzip, zcat and aggregate.read_lines read the members as one stream. Small logs
# that scripts keep appending to (b-gates.log, iouring-window.log, load-holds.log) stay plain.
# The scorecards (*.md) are never compressed.
#
#   bench/review/pack-results.sh        # from the host, never while a batch is writing results
set -euo pipefail
cd "$(dirname "$0")/results"
shopt -s nullglob
for f in *.jsonl *.log *.out; do
    case "$f" in
        *.jsonl) ;;
        *) [ -e "$f.gz" ] || [ "$(stat -c %s "$f")" -gt 262144 ] || continue ;;
    esac
    gzip -9 -n -c "$f" >> "$f.gz"
    rm -- "$f"
    echo "packed $f"
done
