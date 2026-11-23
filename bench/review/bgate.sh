#!/usr/bin/env bash
# Stage B gate for one change, run from the host:
#
#   bench/review/bgate.sh <b-name> [cargo test args...]    e.g. bgate.sh b-url-prefix
#
# 1. rebases the branch of worktree ../wt/<b-name> onto $BASE, perf/stage-b by default (Stage C's
#    bug fixes: BASE=fix/older-bugs), on the host: git inside the container cannot follow a
#    worktree's .git file;
# 2. runs `cargo test --all --locked` (default features, plus any extra args such as
#    `--features mount`) in the bench container -> results/<b-name>-tests.log;
# 3. builds /opt/bins/<b-name> (zigbuild, --features fast, stripped) with build.sh, unless
#    NO_BUILD=1 (a change with no benchmark rows).
# Stops at the first failure. Summary lines go to results/b-gates.log.
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
D="$here/dc"
name="$1"; shift
base="${BASE:-perf/stage-b}"
wt="$here/../../../wt/$name"
log="$here/results/b-gates.log"
tests="$here/results/$name-tests.log"
note() { echo "$(date -u +%FT%TZ) $name $*" | tee -a "$log"; }

git -C "$wt" rebase -q "$base" || { note "REBASE FAILED"; exit 1; }
note "rebased onto $base: $(git -C "$wt" log --format=%h -1) ($(git -C "$wt" rev-list --count "$base"..HEAD) commits)"

# rs5cmd is cleaned first: the target dir is shared by every worktree, and cargo's fingerprint
# of a path package ignores its location, so another worktree's build would look fresh.
"$D" exec -T bench bash -c "cd /src/wt/$name && export CARGO_TARGET_DIR=/target/test-b CARGO_BUILD_JOBS=8 && \
    cargo clean -q -p rs5cmd && cargo test --all --locked $* 2>&1" > "$tests"
rc=$?
passed=$(grep -E '^test result:' "$tests" | awk '{p+=$4; f+=$6} END {print p+0" passed, "f+0" failed"}')
if [ $rc -ne 0 ]; then
    note "TESTS FAILED (exit $rc; $passed): see results/$name-tests.log"
    exit 1
fi
note "tests ok ($passed) $*"
[ -n "${NO_BUILD:-}" ] && { note "not built (NO_BUILD)"; exit 0; }

"$D" exec -T bench bash bench/review/build.sh "$name" >> "$log" 2>&1 || { note "BUILD FAILED"; exit 1; }
note "built /opt/bins/$name"
