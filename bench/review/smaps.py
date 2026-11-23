#!/usr/bin/env python3
"""Peak-memory breakdown of one command (used through matrix.py --wrap).

    smaps.py OUT.json -- command [args...]

Runs the command, samples /proc/<pid>/smaps_rollup every 20 ms and, whenever RSS reaches a new
high (+1 MiB), reads the full /proc/<pid>/smaps and /proc/net/sockstat. Writes the breakdown at
the peak: [heap] (brk), anonymous mappings (malloc arenas, mmap'd buffers, thread stacks other
than the main one), [stack], file-backed (binary and libraries), plus socket-buffer pages and the
thread count. Exits with the command's status.
"""
import json
import os
import subprocess
import sys
import time


def rollup(pid):
    with open(f"/proc/{pid}/smaps_rollup") as f:
        for line in f:
            if line.startswith("Rss:"):
                return int(line.split()[1])
    return 0


def smaps(pid):
    cats = {"heap": 0, "anon": 0, "stack": 0, "file": 0, "other": 0}
    anon_maps = []
    cur = None
    with open(f"/proc/{pid}/smaps") as f:
        for line in f:
            parts = line.split()
            if not parts:
                continue
            if "-" in parts[0] and len(parts) >= 5 and not parts[0].endswith(":"):
                name = parts[5] if len(parts) >= 6 else ""
                lo, hi = (int(x, 16) for x in parts[0].split("-"))
                if name == "[heap]":
                    cur = "heap"
                elif name.startswith("[stack"):
                    cur = "stack"
                elif name == "":
                    cur = "anon"
                elif name.startswith("/"):
                    cur = "file"
                else:
                    cur = "other"
                size_kb = (hi - lo) // 1024
                anon_maps.append([cur, size_kb, 0])
            elif parts[0] == "Rss:" and cur:
                kb = int(parts[1])
                cats[cur] += kb
                anon_maps[-1][2] = kb
    # Histogram of anonymous mappings by virtual size (arenas are 64 MiB, thread stacks 2 MiB+4 KiB).
    hist = {}
    for c, size, rss in anon_maps:
        if c != "anon" or rss == 0:
            continue
        b = "<=132KiB" if size <= 132 else "2MiB-stack" if 2040 <= size <= 2060 else \
            "<=1MiB" if size <= 1024 else "<=8MiB" if size <= 8192 else "<=64MiB" if size <= 65536 else ">64MiB"
        h = hist.setdefault(b, [0, 0])
        h[0] += 1
        h[1] += rss
    return cats, hist


def sockstat():
    out = {}
    try:
        with open("/proc/net/sockstat") as f:
            for line in f:
                if line.startswith("TCP:"):
                    p = line.split()
                    d = dict(zip(p[1::2], p[2::2]))
                    out = {"tcp_inuse": int(d.get("inuse", 0)), "tcp_mem_pages": int(d.get("mem", 0))}
    except OSError:
        pass
    return out


def sock_queues(pid):
    """(sockets, queued receive + send bytes) over the process's TCP sockets: the rx_queue and
    tx_queue columns of /proc/<pid>/net/tcp{,6}, for the socket inodes among its fds. Queued
    bytes, not the kernel's skb truesize, but per process (sockstat's figure is per netns)."""
    inodes = set()
    try:
        for fd in os.listdir(f"/proc/{pid}/fd"):
            try:
                t = os.readlink(f"/proc/{pid}/fd/{fd}")
            except OSError:
                continue
            if t.startswith("socket:["):
                inodes.add(t[8:-1])
    except OSError:
        return 0, 0
    n = q = 0
    for name in ("tcp", "tcp6"):
        try:
            with open(f"/proc/{pid}/net/{name}") as f:
                next(f)
                for line in f:
                    c = line.split()
                    if len(c) > 9 and c[9] in inodes:
                        tx, rx = c[4].split(":")
                        n += 1
                        q += int(tx, 16) + int(rx, 16)
        except OSError:
            pass
    return n, q


def threads(pid):
    try:
        return len(os.listdir(f"/proc/{pid}/task"))
    except OSError:
        return 0


def main():
    out = sys.argv[1]
    cmd = sys.argv[sys.argv.index("--") + 1:]
    p = subprocess.Popen(cmd)
    peak, best = 0, None
    sq_max, socks_max, i = 0, 0, 0
    t0 = time.time()
    while p.poll() is None:
        i += 1
        if i % 5 == 0:
            n, q = sock_queues(p.pid)
            sq_max, socks_max = max(sq_max, q), max(socks_max, n)
        try:
            r = rollup(p.pid)
            if r > peak + 1024:
                cats, hist = smaps(p.pid)
                best = {"rss_kb": r, "t_s": round(time.time() - t0, 3), "categories_kb": cats,
                        "anon_hist": hist, "sock": sockstat(), "threads": threads(p.pid)}
            peak = max(peak, r)
        except (OSError, ValueError):
            pass
        time.sleep(0.02)
    rec = {"cmd": cmd, "peak_rss_kb_sampled": peak, "at_peak": best, "exit": p.returncode,
           "sock_queued_kb_max": sq_max // 1024, "sockets_max": socks_max}
    with open(out, "w") as f:
        json.dump(rec, f)
    sys.exit(p.returncode)


if __name__ == "__main__":
    main()
