#!/usr/bin/env bash
# Compress the raw results of finished soak runs before committing them, in the style of
# bench/review/pack-results.sh: every results/<run>/*.jsonl with `gzip -9 -n` (no name or time
# stamp, so the same input gives the same bytes). summary.md, meta.json and state.json stay plain.
#
#   bench/soak/pack-results.sh        # from the host, never while a run is writing results
set -euo pipefail
cd "$(dirname "$0")/results"
shopt -s nullglob
for f in */*.jsonl; do
    gzip -9 -n -c "$f" >> "$f.gz"
    rm -- "$f"
    echo "packed $f"
done
