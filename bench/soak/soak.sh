#!/usr/bin/env bash
# Host-side entry point of the soak. One run at a time.
#
#   bench/soak/soak.sh dry [runner options]     dry run against the MinIO sidecar
#   bench/soak/soak.sh aws [runner options]     needs ~/.config/rs5cmd-soak/aws.env
#   bench/soak/soak.sh rgw [runner options]     needs ~/.config/rs5cmd-soak/rgw.env
#   bench/soak/soak.sh <target> cleanup RUN-ID  delete what an interrupted run left at the endpoint
#   bench/soak/soak.sh down                     remove the containers, the network and the volumes
#
# Runner options are passed to soak.py run (--scale, --only, --skip, --client-bin ...). The scale
# defaults to `dry` for the dry run and `full` for the real endpoints.
#
# SOAK_RS5CMD_BIN names the rs5cmd build in the review's bins volume (default b-stage). A build
# with the data-path switch runs as rs5cmd-sdk, rs5cmd-lean and rs5cmd-lean-tr. SOAK_BINS=keep
# reuses the copies of the previous run.
#
# What this script owns, in order:
#   1. the containers (compose project rs5cmd-soak) and a check of their limits: cpuset 3,11 on
#      every container, memory limits of all of them together at most 2 GiB, io.max in place
#   2. the binaries: copied from the review's bins volume (read-only), nothing is compiled
#   3. the rate limit: tc.sh apply, removed again by a trap on every way out, then checked
#   4. the host guard, every 10 s: conntrack count (warn above 10,000; above 30,000 the soak
#      containers are killed), the rate limit still in place, rate requests of the runner
#      (only ever downwards from the cap)
#   5. the runner inside the soak container
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
dc="$here/dc"
run="$here/.run"
PROJECT=rs5cmd-soak
CT_WARN=10000
CT_ABORT=30000
MEM_MAX=$((2 * 1024 * 1024 * 1024))
bin="${SOAK_RS5CMD_BIN:-b-stage}"
[[ "$bin" =~ ^[A-Za-z0-9._-]+$ ]] || { echo "soak.sh: STOP: bad SOAK_RS5CMD_BIN" >&2; exit 3; }
# Test hook: a lower abort threshold (to try the abort path). It can never raise the limit.
if [[ "${SOAK_TEST_CT_ABORT:-}" =~ ^[0-9]+$ ]] && [ "$SOAK_TEST_CT_ABORT" -lt "$CT_ABORT" ]; then
    CT_ABORT="$SOAK_TEST_CT_ABORT"
fi
mkdir -p "$run" "$here/results"

say() { echo "soak.sh: $*"; }
die() { echo "soak.sh: STOP: $*" >&2; exit 3; }
containers() { sudo -n docker ps -q --filter "label=com.docker.compose.project=$PROJECT"; }

target="${1:?usage: soak.sh dry|aws|rgw|down [runner options]}"
shift
if [ "$target" = down ]; then
    "$here/tc.sh" remove || true
    "$dc" --profile dry down -v
    exit 0
fi
case "$target" in dry|aws|rgw) ;; *) die "unknown target '$target'" ;; esac
if [ "$target" != dry ] && [ ! -f "${SOAK_CREDS_DIR:-$HOME/.config/rs5cmd-soak}/$target.env" ]; then
    die "the credentials file for '$target' is not there: waiting for credentials"
fi

exec 9>"$run/lock"
flock -n 9 || die "another soak run is active"
rm -f "$run/abort" "$run/guard.json" "$run/rate.request"

# 1. containers and their limits
if [ "$target" = dry ]; then
    "$dc" --profile dry up -d --wait
    "$here/dry-setup.sh"
else
    "$dc" up -d --wait soak
fi
total=0
for c in $(containers); do
    read -r name cpus mem swap <<<"$(sudo -n docker inspect -f '{{.Name}} {{.HostConfig.CpusetCpus}} {{.HostConfig.Memory}} {{.HostConfig.MemorySwap}}' "$c")"
    [ "$cpus" = "3,11" ] || die "$name has cpuset '$cpus', not 3,11"
    [ "$mem" -gt 0 ] && [ "$swap" = "$mem" ] || die "$name has no hard memory limit"
    pid="$(sudo -n docker inspect -f '{{.State.Pid}}' "$c")"
    cg="/sys/fs/cgroup$(awk -F: '{print $3}' "/proc/$pid/cgroup")"
    [ "$(sudo -n cat "$cg/cpuset.cpus.effective")" = "3,11" ] || die "$name: effective cpuset is not 3,11"
    sudo -n cat "$cg/io.max" | grep -q 'rbps=314572800 wbps=209715200 riops=3000 wiops=2000' || die "$name: io.max is not set"
    total=$((total + mem))
done
[ "$total" -le "$MEM_MAX" ] || die "memory limits add up to $total bytes, above 2 GiB"
say "limits ok: $(containers | wc -l) container(s), cpuset 3,11, memory limits $((total / 1024 / 1024)) MiB in all, io.max set"

# 2. binaries (copied, never built) and where rs5cmd's came from
# (SOAK_BINS=keep reuses the copies of the previous run, so that two endpoints get the same build.)
if [ "${SOAK_BINS:-copy}" = keep ]; then say "keeping the binaries copied earlier"; else "$dc" exec -T -e SOAK_RS5CMD_BIN="$bin" soak python3 bench/soak/soak.py prepare; fi
# The build's worktree has the binary's name (bench/review/build.sh): its branch and commit now.
if [ "${SOAK_BINS:-copy}" = keep ] && [ -s "$run/rs5cmd-src" ]; then src="$(cat "$run/rs5cmd-src")"; else
    wt="$here/../../../$bin"
    src="${SOAK_RS5CMD_SRC:-$(git -C "$wt" rev-parse --abbrev-ref HEAD 2>/dev/null || true) $(TZ=UTC git -C "$wt" log -1 --date=format-local:%Y-%m-%dT%H:%M:%SZ --format='%h %cd' 2>/dev/null || true)}"
    echo "$src" > "$run/rs5cmd-src"
fi
image_id="$(sudo -n docker inspect -f '{{.Image}}' "$(sudo -n docker ps -q --filter "label=com.docker.compose.project=$PROJECT" --filter label=com.docker.compose.service=soak)")"

# 3. the rate limit, with its removal on every way out
guard_pid=""
finish() {
    rc=$?
    trap - EXIT INT TERM
    [ -z "$guard_pid" ] || kill "$guard_pid" 2>/dev/null || true
    # A container that the guard has just killed may still be going away: try again before giving up.
    removed=no
    for _ in 1 2 3 4; do
        if "$here/tc.sh" remove; then removed=yes; break; fi
        sleep 3
    done
    if [ "$removed" = yes ]; then say "rate limit removed and checked"; else say "!! the rate limit removal check FAILED: look at tc.sh status"; rc=4; fi
    rm -f "$run/guard.json" "$run/rate.request"
    exit "$rc"
}
trap finish EXIT
trap 'exit 130' INT TERM
"$here/tc.sh" apply

# 4. the host guard
kill_soak() {
    echo "$1" > "$run/abort"
    echo "$(date -u +%FT%TZ) ABORT: $1" >> "$run/guard.log"
    # shellcheck disable=SC2046
    sudo -n docker kill $(containers) >/dev/null 2>&1 || true
}
guard() {
    local n=0 ct load cur tc_ok req
    while :; do
        if [ -s "$run/rate.request" ]; then
            req="$(head -c 16 "$run/rate.request" | tr -d '\n ')"
            rm -f "$run/rate.request"
            if "$here/tc.sh" rate "$req" 2>> "$run/guard.log"; then n=0; else kill_soak "rate request '$req' failed"; return; fi
        fi
        if [ $((n % 10)) = 0 ]; then
            ct="$(cat /proc/sys/net/netfilter/nf_conntrack_count)"
            load="$(cut -d' ' -f1 /proc/loadavg)"
            if cur="$("$here/tc.sh" verify 2>> "$run/guard.log")" || { sleep 1; cur="$("$here/tc.sh" verify 2>> "$run/guard.log")"; }; then
                tc_ok=true
            else
                tc_ok=false; cur=0
            fi
            printf '{"ts": %s, "conntrack": %s, "load1": %s, "tc_ok": %s, "tc_rate": %s}\n' \
                "$(date +%s)" "$ct" "$load" "$tc_ok" "$cur" > "$run/guard.json.tmp"
            mv "$run/guard.json.tmp" "$run/guard.json"
            if [ "$ct" -gt "$CT_ABORT" ]; then kill_soak "host conntrack count $ct above $CT_ABORT"; return; fi
            if [ "$ct" -gt "$CT_WARN" ]; then echo "$(date -u +%FT%TZ) WARN: conntrack count $ct above $CT_WARN" >> "$run/guard.log"; fi
            if [ "$tc_ok" = false ]; then kill_soak "the rate limit is not in place"; return; fi
        fi
        n=$((n + 1))
        sleep 1
    done
}
guard &
guard_pid=$!
for _ in $(seq 20); do [ -s "$run/guard.json" ] && break; sleep 0.5; done
[ -s "$run/guard.json" ] || die "the host guard did not start"

# 5. the runner
scale=full
[ "$target" = dry ] && scale=dry
case " $* " in *" --scale "*) scale_args=() ;; *) scale_args=(--scale "$scale") ;; esac
log="$run/soak-$target.log"   # console log, not part of the results
set +e
if [ "${1:-}" = cleanup ]; then
    "$dc" exec -T -e SOAK_RS5CMD_BIN="$bin" soak python3 -u bench/soak/soak.py cleanup --target "$target" --run-id "${2:?run id}" 2>&1 | tee "$log"
else
    "$dc" exec -T -e SOAK_RS5CMD_BIN="$bin" -e SOAK_RS5CMD_SRC="$src" -e SOAK_IMAGE_ID="$image_id" soak \
        python3 -u bench/soak/soak.py run --target "$target" "${scale_args[@]}" "$@" 2>&1 | tee "$log"
fi
rc=${PIPESTATUS[0]}
set -e
[ ! -s "$run/abort" ] || say "!! aborted by the host guard: $(cat "$run/abort")"
say "runner exit code $rc"
exit "$rc"
