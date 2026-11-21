#!/usr/bin/env bash
# Host side: run a batch of `--fast` work inside a restricted io_uring window, then close it.
#
#   bench/review/fast-window.sh <command...>      # e.g. bench/review/dc exec -T bench bash ...
#
# On hosts where io_uring is disabled (kernel.io_uring_disabled=2), `rs5cmd --fast` silently
# falls back to epoll. This script opens the narrowest window that lets the bench container
# use io_uring:
#   kernel.io_uring_group=<gid>   only members of this gid (the bench container's group_add)
#   kernel.io_uring_disabled=1    io_uring only for that group and CAP_SYS_ADMIN processes
# and restores the original values on exit, whatever happens (trap). Open and close times go to
# results/iouring-window.log. A host-side probe, run as the calling (ordinary) user, must report
# EPERM both while the window is open and after it is closed.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
gid="${IOURING_GID:-59900}"
log="$here/results/iouring-window.log"
mkdir -p "$here/results"
probe="$(mktemp -d)/iouring-probe"

stamp() { date -u +%Y-%m-%dT%H:%M:%SZ; }
note() { echo "$(stamp) $*" | tee -a "$log" >&2; }

# The probe binary is built (static) in the bench container; copy it out to run it on the host.
sudo docker cp rs5cmd-optreview-bench-1:/opt/bins/iouring-probe - | tar -xO > "$probe"
chmod +x "$probe"

orig_disabled="$(sysctl -n kernel.io_uring_disabled)"
orig_group="$(sysctl -n kernel.io_uring_group)"

host_probe() {
    local r
    r="$("$probe" || true)"
    note "host probe ($1, ordinary user): $r"
    case "$r" in *"not permitted"*) ;; *) note "WARNING: host probe did not get EPERM";; esac
}

close_window() {
    sudo sysctl -q -w kernel.io_uring_disabled="$orig_disabled"
    sudo sysctl -q -w kernel.io_uring_group="$orig_group"
    note "window CLOSED: io_uring_disabled=$(sysctl -n kernel.io_uring_disabled) io_uring_group=$(sysctl -n kernel.io_uring_group)"
    host_probe "after close"
    rm -f "$probe"
}
trap close_window EXIT INT TERM

sudo sysctl -q -w kernel.io_uring_group="$gid"
sudo sysctl -q -w kernel.io_uring_disabled=1
note "window OPEN: io_uring_disabled=1 io_uring_group=$gid (was $orig_disabled / $orig_group)"
host_probe "while open"
# The command itself is not logged: it can carry local paths. Callers log what they run.
note "running the batch (pid $$)"
"$@"
