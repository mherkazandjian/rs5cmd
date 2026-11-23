#!/usr/bin/env bash
# Matrix entry point, run inside the pinned bench container:
#   bash bench/review/run-matrix.sh run --rows 'W1.*' --endpoint stub --clients s5cmd,rs5cmd ...
#   bash bench/review/run-matrix.sh list
# See matrix.py for the options and README.md for the full sequence.
exec env -u PYTHONSTARTUP python3 -u "$(dirname "$0")/matrix.py" "$@"
