#!/usr/bin/env python3
"""Scheduler view of one command: on-CPU time, run-queue wait and timeslices, summed over its
threads from /proc/<pid>/task/<tid>/schedstat (sampled every 10 ms; a thread's last sample counts
after it exits, so up to 10 ms of a short-lived thread's time can be missed).

    python3 schedstat.py <out.json> -- <command...>

Used instead of `perf sched record`, which traces every CPU of the host (other users' tasks
included). Off-CPU time per thread = its lifetime - on-CPU - run-queue wait (sleeping or blocked).
"""

import json
import os
import subprocess
import sys
import time


def sample(pid, seen, born, now):
    try:
        tids = os.listdir(f"/proc/{pid}/task")
    except OSError:
        return
    for tid in tids:
        try:
            with open(f"/proc/{pid}/task/{tid}/schedstat") as f:
                on, wait, slices = (int(x) for x in f.read().split()[:3])
        except (OSError, ValueError):
            continue
        seen[tid] = (on, wait, slices, now)
        born.setdefault(tid, now)


def main():
    out = sys.argv[1]
    cmd = sys.argv[sys.argv.index("--") + 1:]
    seen, born = {}, {}
    t0 = time.monotonic()
    p = subprocess.Popen(cmd)
    while p.poll() is None:
        sample(p.pid, seen, born, time.monotonic())
        time.sleep(0.01)
    wall = time.monotonic() - t0
    on = sum(v[0] for v in seen.values()) / 1e9
    wait = sum(v[1] for v in seen.values()) / 1e9
    slices = sum(v[2] for v in seen.values())
    life = sum(seen[t][3] - born[t] for t in seen)
    rec = {"cmd": cmd, "exit": p.returncode, "wall_s": round(wall, 3), "threads": len(seen),
           "on_cpu_s": round(on, 3), "runq_wait_s": round(wait, 3), "timeslices": slices,
           "off_cpu_s": round(max(0.0, life - on - wait), 3),
           "wait_per_slice_us": round(1e6 * wait / slices, 1) if slices else None}
    with open(out, "w") as f:
        json.dump(rec, f)
    sys.exit(p.returncode)


if __name__ == "__main__":
    main()
