#!/usr/bin/env python3
"""Compact scorecard for docs/OPTIMIZATION-REVIEW.md, from matrix.py JSONL results.

    python3 bench/review/report.py results/a3.jsonl [--extra results/a3-uring.jsonl]
        [--endpoint stub] [--rows 'W1.*'] [--clients rs5cmd,rs5cmd-fast-epoll,rs5cmd-fast-uring]

One line per (row, endpoint, variant): the baseline client's (s5cmd) median wall, CPU and peak
RSS, then for each other client `wall× CPU× RSS×` (median of per-round ratios), each followed by
its verdict: W (win), L (lose), = (ranges overlap). Same rejection rules as aggregate.py.

Results passed with --extra were not interleaved with the baseline (e.g. the io_uring window
batch). For those, the ratio is median / baseline median and the cell is marked with †.
"""

import argparse
import fnmatch
import re
from collections import defaultdict

import aggregate as ag

MARK = {"win": "W", "lose": "L", "tie": "=", "n/a": "?"}


def natural(row):
    return [int(t) if t.isdigit() else t for t in re.split(r"(\d+)", row)]


def cell(s, base, interleaved):
    if not s:
        return ""
    if not s.get("n"):
        return f"FAILED {s['failed']}" if s.get("failed") else f"rejected {s['rejected']}"
    out = []
    for m in ag.METRICS:
        if interleaved:
            r = ag.ratio(s, base, m)
        else:
            r = s[m]["median"] / base[m]["median"] if base[m]["median"] > 0 else None
        out.append(f"{ag.fmt(r)}{MARK[ag.verdict(s, base, m)]}")
    txt = " ".join(out)
    if not interleaved:
        txt += " †"
    if s.get("failed"):
        txt += f" ({s['failed']} failed)"
    return txt


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("files", nargs="+")
    ap.add_argument("--extra", action="append", default=[])
    ap.add_argument("--endpoint", default="*")
    ap.add_argument("--rows", default="*")
    ap.add_argument("--baseline", default="s5cmd")
    ap.add_argument("--clients", default="rs5cmd,rs5cmd-fast-epoll,rs5cmd-fast-uring")
    ap.add_argument("--max-other", type=float, default=0.35)
    ap.add_argument("--spike", type=float, default=0.20)
    ap.add_argument("--other-cpus", default="0-3,8-11")
    a = ap.parse_args()

    def pick(paths):
        return [r for r in ag.load(paths)
                if any(fnmatch.fnmatch(r["row"], p) for p in a.rows.split(","))
                and fnmatch.fnmatch(r["endpoint"], a.endpoint)]

    n_other = ag.ncpus(a.other_cpus)
    main_s = ag.summarize(pick(a.files), a.max_other, n_other, a.spike)
    extra_s = ag.summarize(pick(a.extra), a.max_other, n_other, a.spike) if a.extra else {}
    clients = a.clients.split(",")

    groups = defaultdict(dict)
    for (row, ep, var, client), s in main_s.items():
        groups[(row, ep, var)][client] = (s, True)
    for (row, ep, var, client), s in extra_s.items():
        groups[(row, ep, var)].setdefault(client, (s, False))

    print(f"| row | endpoint | {a.baseline}: wall s / CPU s / RSS MB | "
          + " | ".join(f"{c}: wall× CPU× RSS×" for c in clients) + " |")
    print("|---|---|---:|" + "---|" * len(clients))
    for key in sorted(groups, key=lambda k: (k[1], natural(k[0]), k[2])):
        row, ep, var = key
        g = groups[key]
        base = g.get(a.baseline, (None, True))[0]
        if not base or not base.get("n"):
            continue
        if not any(c in g for c in clients):
            continue
        b = (f"{ag.fmt(base['wall_s']['median'], 3)} / {ag.fmt(base['cpu_s']['median'])} / "
             f"{ag.fmt(base['maxrss_mb']['median'], 1)}")
        cells = [cell(g[c][0], base, g[c][1]) if c in g else "" for c in clients]
        label = f"{row}{(' ' + var) if var else ''}"
        print(f"| {label} | {ep} | {b} | " + " | ".join(cells) + " |")


if __name__ == "__main__":
    main()
