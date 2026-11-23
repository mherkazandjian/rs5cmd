#!/usr/bin/env bash
# Rate limit for the soak container, both directions, with tbf. Host side.
#
#   tc.sh apply [MBPS]    shape both directions (default and maximum: 40 MB/s = 320 Mbit/s)
#   tc.sh rate MBPS       change the rate of the existing qdiscs (0.5 .. 40)
#   tc.sh verify [MBPS]   exit 0 only if both qdiscs are in place (with that rate, if given)
#   tc.sh status          print what is found, change nothing
#   tc.sh remove          delete both qdiscs and check that they are gone
#
# Exactly two interfaces are ever written to:
#   uploads    eth0 inside the soak container's network namespace (nsenter -t <pid> -n)
#   downloads  the host-side veth of that eth0
# The veth is looked up, never guessed: the container's eth0 iflink (read two independent ways)
# must equal the ifindex of exactly one host interface; that interface must be a veth, a port of
# the soak project's bridge, and point back at the container's eth0; and the bridge's ports must
# be exactly the eth0 peers of the project's running containers. Any failed assertion stops the
# script (exit 3) before a qdisc is touched. Reading is unrestricted; writing happens only in
# tc_write_container and tc_write_veth.
#
# Note: `nsenter -n cat /sys/class/net/eth0/iflink` does not work for this lookup. /sys shows
# the interfaces of the namespace that mounted it (the host's), so the file is not there. The
# container's own /sys (docker exec) and netlink inside the namespace (ip) are used instead.
set -euo pipefail

PROJECT=rs5cmd-soak
SERVICE=soak
BRIDGE=br-rs5cmd-soak
NET="${PROJECT}_net"
MAX_MBPS=40
MIN_MBPS=0.5
BURST=512kb
LATENCY=100ms

here="$(cd "$(dirname "$0")" && pwd)"
run_dir="$here/.run"
mkdir -p "$run_dir"

die() { echo "tc.sh: STOP: $*" >&2; exit 3; }
note() { echo "tc.sh: $*" >&2; }

# All host qdiscs as sorted "dev kind handle" lines (read-only).
host_qdiscs() { tc -j qdisc show | jq -r '.[] | "\(.dev) \(.kind) \(.handle)"' | sort; }

# Sets: cid pid ct_ns ct_ifindex ct_iflink veth
discover() {
    local ids nets host_ns j names b_iflink b_ifindex n kind master opt
    ids="$(sudo -n docker ps -q --no-trunc \
        --filter "label=com.docker.compose.project=$PROJECT" \
        --filter "label=com.docker.compose.service=$SERVICE")"
    [ "$(wc -w <<<"$ids")" = 1 ] || die "expected one running $PROJECT/$SERVICE container, found $(wc -w <<<"$ids")"
    cid="$ids"
    pid="$(sudo -n docker inspect -f '{{.State.Pid}}' "$cid")"
    [[ "$pid" =~ ^[1-9][0-9]*$ ]] || die "container has no pid"
    grep -q "$cid" "/proc/$pid/cgroup" || die "pid $pid is not in the container's cgroup"
    nets="$(sudo -n docker inspect -f '{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{end}}' "$cid")"
    [ "$nets" = "$NET " ] || die "container networks are '$nets', expected only $NET"
    host_ns="$(sudo -n readlink /proc/1/ns/net)"
    ct_ns="$(sudo -n readlink "/proc/$pid/ns/net")"
    [ -n "$ct_ns" ] && [ -n "$host_ns" ] && [ "$ct_ns" != "$host_ns" ] \
        || die "container network namespace not distinct from the host's"

    # (a) the container's own sysfs
    ct_iflink="$(sudo -n docker exec "$cid" cat /sys/class/net/eth0/iflink)"
    ct_ifindex="$(sudo -n docker exec "$cid" cat /sys/class/net/eth0/ifindex)"
    [[ "$ct_iflink" =~ ^[1-9][0-9]*$ && "$ct_ifindex" =~ ^[1-9][0-9]*$ ]] || die "cannot read eth0 iflink / ifindex in the container"
    # (b) netlink inside the namespace
    j="$(sudo -n nsenter -t "$pid" -n ip -j link show)"
    names="$(jq -r '[.[].ifname] | sort | join(",")' <<<"$j")"
    [ "$names" = "eth0,lo" ] || die "interfaces in the container namespace are '$names', expected eth0,lo"
    b_iflink="$(jq -r '.[] | select(.ifname=="eth0") | .link_index' <<<"$j")"
    b_ifindex="$(jq -r '.[] | select(.ifname=="eth0") | .ifindex' <<<"$j")"
    [ "$b_iflink" = "$ct_iflink" ] && [ "$b_ifindex" = "$ct_ifindex" ] \
        || die "eth0 iflink/ifindex disagree: sysfs $ct_iflink/$ct_ifindex, netlink $b_iflink/$b_ifindex"

    # host side: exactly one interface with that ifindex
    veth="$(ip -j link show | jq -r --argjson i "$ct_iflink" '.[] | select(.ifindex==$i) | .ifname')"
    n="$(grep -c . <<<"$veth" || true)"
    [ "$n" = 1 ] || die "host interfaces with ifindex $ct_iflink: $n (expected 1)"
    assert_veth
    # closed world: the bridge's ports are exactly the eth0 peers of the project's containers
    local want have c
    want="$(for c in $(sudo -n docker ps -q --filter "label=com.docker.compose.project=$PROJECT"); do
                sudo -n docker exec "$c" cat /sys/class/net/eth0/iflink; done | sort -n | tr '\n' ' ')"
    have="$(for c in "/sys/class/net/$BRIDGE/brif"/*; do cat "/sys/class/net/$(basename "$c")/ifindex"; done | sort -n | tr '\n' ' ')"
    [ "$want" = "$have" ] || die "ports of $BRIDGE ($have) are not the project's containers ($want)"
    opt="$(sudo -n docker network inspect -f '{{index .Options "com.docker.network.bridge.name"}}' "$NET")"
    [ "$opt" = "$BRIDGE" ] || die "docker network $NET uses bridge '$opt', expected $BRIDGE"
    sudo -n docker network inspect -f '{{range $k, $v := .Containers}}{{$k}} {{end}}' "$NET" | grep -q "$cid" \
        || die "container is not listed on docker network $NET"
}

# Everything that must hold for $veth, re-checked right before every write to it.
assert_veth() {
    local kind master
    [[ "$veth" == veth* ]] || die "host interface '$veth' is not named veth*"
    [ -d "/sys/class/net/$veth" ] || die "$veth is gone"
    [ "$(cat "/sys/class/net/$veth/ifindex")" = "$ct_iflink" ] || die "$veth ifindex changed"
    [ "$(cat "/sys/class/net/$veth/iflink")" = "$ct_ifindex" ] || die "$veth does not point back at the container's eth0"
    [ ! -e "/sys/class/net/$veth/device" ] || die "$veth has a hardware device behind it"
    [ -d "/sys/class/net/$veth/brport" ] || die "$veth is not a bridge port"
    [ "$(basename "$(readlink "/sys/class/net/$veth/master")")" = "$BRIDGE" ] || die "$veth master is not $BRIDGE"
    kind="$(ip -d -j link show dev "$veth" | jq -r '.[0].linkinfo.info_kind')"
    [ "$kind" = veth ] || die "$veth is of kind '$kind', not veth"
    master="$(ip -j link show dev "$veth" | jq -r '.[0].master')"
    [ "$master" = "$BRIDGE" ] || die "$veth master (netlink) is '$master', not $BRIDGE"
    kind="$(ip -d -j link show dev "$BRIDGE" | jq -r '.[0].linkinfo.info_kind')"
    [ "$kind" = bridge ] || die "$BRIDGE is of kind '$kind', not bridge"
}

# The two write paths. $@ = tc arguments after `tc qdisc`, without `dev`.
tc_write_container() {
    local verb="$1"; shift
    # shellcheck disable=SC2016
    sudo -n nsenter -t "$pid" -n sh -c \
        'test "$(readlink /proc/self/ns/net)" = "$1" || { echo "wrong namespace" >&2; exit 3; }
         shift; verb="$1"; shift; exec tc qdisc "$verb" dev eth0 "$@"' sh "$ct_ns" "$verb" "$@"
}
tc_write_veth() {
    local verb="$1"; shift
    assert_veth
    sudo -n tc qdisc "$verb" dev "$veth" "$@"
}

# Root qdisc as "kind rate_bytes_per_s" (rate empty unless tbf).
root_qdisc_container() {
    sudo -n nsenter -t "$pid" -n tc -j qdisc show dev eth0 | jq -r '.[] | select(.root==true) | "\(.kind) \(.options.rate // "")"'
}
root_qdisc_veth() {
    tc -j qdisc show dev "$veth" | jq -r '.[] | select(.root==true) | "\(.kind) \(.options.rate // "")"'
}

rate_bytes() {  # MBPS -> bytes/s, validated
    local m="$1"
    [[ "$m" =~ ^[0-9]+(\.[0-9]+)?$ ]] || die "bad rate '$m'"
    awk -v m="$m" -v lo="$MIN_MBPS" -v hi="$MAX_MBPS" 'BEGIN { exit !(m >= lo && m <= hi) }' \
        || die "rate $m MB/s outside $MIN_MBPS..$MAX_MBPS"
    awk -v m="$m" 'BEGIN { printf "%d", m * 1000000 }'
}

set_rate() {
    local bytes="$1" bits=$(( $1 * 8 ))
    tc_write_container replace root handle 1: tbf rate "${bits}bit" burst "$BURST" latency "$LATENCY"
    tc_write_veth replace root handle 1: tbf rate "${bits}bit" burst "$BURST" latency "$LATENCY"
    [ "$(root_qdisc_container)" = "tbf $bytes" ] || die "container eth0 root qdisc is '$(root_qdisc_container)' after apply"
    [ "$(root_qdisc_veth)" = "tbf $bytes" ] || die "$veth root qdisc is '$(root_qdisc_veth)' after apply"
    printf '%s %s %s %s\n' "$veth" "$ct_iflink" "$cid" "$bytes" > "$run_dir/tc.state"
    note "shaped: container eth0 (uploads) and $veth (downloads) at $bytes bytes/s each"
}

cmd="${1:-status}"
case "$cmd" in apply|rate|remove)
    # One writer at a time: a rate change that is under way finishes before a removal starts, and
    # one that starts after it finds no qdisc to change and stops.
    exec 8>"$run_dir/tc.lock"
    flock 8 ;;
esac
case "$cmd" in
apply)
    bytes="$(rate_bytes "${2:-$MAX_MBPS}")"
    discover
    [ -e "$run_dir/qdisc.before" ] || host_qdiscs > "$run_dir/qdisc.before"
    set_rate "$bytes"
    ;;
rate)
    bytes="$(rate_bytes "${2:?rate in MB/s}")"
    discover
    [[ "$(root_qdisc_container)" == tbf\ * && "$(root_qdisc_veth)" == tbf\ * ]] || die "no qdiscs to change: run apply first"
    set_rate "$bytes"
    ;;
verify)
    discover
    c="$(root_qdisc_container)"; v="$(root_qdisc_veth)"
    if [ -n "${2:-}" ]; then
        bytes="$(rate_bytes "$2")"
        [ "$c" = "tbf $bytes" ] && [ "$v" = "tbf $bytes" ] || die "qdiscs are '$c' / '$v', expected tbf $bytes"
    else
        [[ "$c" == tbf\ * && "$v" == tbf\ * ]] || die "qdiscs are '$c' / '$v', expected tbf"
        [ "${c#tbf }" -le $(( MAX_MBPS * 1000000 )) ] && [ "${v#tbf }" -le $(( MAX_MBPS * 1000000 )) ] || die "rate above the maximum"
    fi
    echo "${c#tbf }"
    ;;
status)
    discover
    echo "container  pid $pid  eth0 ifindex $ct_ifindex -> host ifindex $ct_iflink = $veth (port of $BRIDGE)"
    echo "container eth0 root qdisc: $(root_qdisc_container)"
    echo "$veth root qdisc: $(root_qdisc_veth)"
    ;;
remove)
    rc=0
    ids="$(sudo -n docker ps -q --filter "label=com.docker.compose.project=$PROJECT" --filter "label=com.docker.compose.service=$SERVICE")"
    if [ -n "$ids" ]; then
        discover
        if [[ "$(root_qdisc_container)" == tbf\ * ]]; then tc_write_container del root; fi
        if [[ "$(root_qdisc_veth)" == tbf\ * ]]; then tc_write_veth del root; fi
        [[ "$(root_qdisc_container)" != tbf\ * ]] || { note "container eth0 still has tbf"; rc=4; }
        [[ "$(root_qdisc_veth)" != tbf\ * ]] || { note "$veth still has tbf"; rc=4; }
    else
        note "no running soak container: its interfaces and qdiscs went with it"
        if [ -s "$run_dir/tc.state" ]; then
            read -r old_veth old_idx _ < "$run_dir/tc.state"
            if [ -d "/sys/class/net/$old_veth" ] && [ "$(cat "/sys/class/net/$old_veth/ifindex")" = "$old_idx" ]; then
                note "$old_veth (ifindex $old_idx) still exists without its container: not touched, look at it"
                rc=4
            fi
        fi
    fi
    # No tbf anywhere on the host that was not there before the first apply.
    if [ -e "$run_dir/qdisc.before" ]; then
        extra="$(comm -13 "$run_dir/qdisc.before" <(host_qdiscs) | grep ' tbf ' || true)"
        [ -z "$extra" ] || { note "tbf qdiscs on the host that were not there before: $extra"; rc=4; }
    fi
    if [ "$rc" = 0 ]; then
        rm -f "$run_dir/tc.state" "$run_dir/qdisc.before"
        note "removed: no soak qdisc left"
    fi
    exit "$rc"
    ;;
*)
    echo "usage: tc.sh apply [MBPS] | rate MBPS | verify [MBPS] | status | remove" >&2
    exit 2
    ;;
esac
