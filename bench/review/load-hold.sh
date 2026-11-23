#!/usr/bin/env bash
# Holds a running matrix.py between rounds while the host's 1-minute load average is above
# LOAD_MAX (default 11), for batches started before matrix.py waited without a time limit.
# Run on the host:  bench/review/load-hold.sh <pid that ends the hold, e.g. a3.sh>
# SIGSTOP is sent only when matrix.py's sole children are s3stub (no measured command or setup
# step running); SIGCONT once the load is back at or below LOAD_MAX. Log: results/load-holds.log.
set -u
until_pid="$1"
max="${LOAD_MAX:-11}"
log="$(cd "$(dirname "$0")" && pwd)/results/load-holds.log"
while kill -0 "$until_pid" 2>/dev/null; do
    pid=$(pgrep -f 'python3 -u bench/review/matrix.py' | head -1)
    if [ -n "$pid" ]; then
        load=$(cut -d' ' -f1 /proc/loadavg)
        state=$(ps -o stat= -p "$pid" | tr -d ' ')
        busy=0
        for c in $(pgrep -P "$pid"); do
            [ "$(cat /proc/$c/comm 2>/dev/null)" = s3stub ] || busy=1
        done
        if awk -v l="$load" -v m="$max" 'BEGIN { exit !(l > m) }'; then
            if [[ $state != T* ]] && [ "$busy" = 0 ]; then
                sudo kill -STOP "$pid" && echo "$(date -u +%FT%TZ) hold pid=$pid load1=$load" >> "$log"
            fi
        elif [[ $state == T* ]]; then
            sudo kill -CONT "$pid" && echo "$(date -u +%FT%TZ) release pid=$pid load1=$load" >> "$log"
        fi
    fi
    sleep 10
done
pid=$(pgrep -f 'python3 -u bench/review/matrix.py' | head -1)
[ -n "$pid" ] && sudo kill -CONT "$pid" 2>/dev/null
echo "$(date -u +%FT%TZ) done" >> "$log"
