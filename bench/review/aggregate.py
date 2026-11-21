#!/usr/bin/env python3
"""Turn matrix.py JSONL results into markdown tables.

    python3 bench/review/aggregate.py results/*.jsonl.gz [--max-other 0.35] [--rows 'W1.*']
        [--endpoint stub] [--baseline s5cmd] [--format md|json]

Per (row, endpoint, variant, client), over the measured rounds (warm-ups and failed runs
excluded):
  - wall and CPU (user+sys): best, median and min-max; user and sys medians
  - peak RSS: median and max (never best)
  - ratio vs the baseline client: median of the per-round ratios (rounds are interleaved, so
    round k of each client ran back to back)
  - verdict per metric: `win` / `lose` only when the two clients' min-max ranges do not overlap,
    otherwise `tie` (a difference counts only if it exceeds the spread)
  - rounds rejected for neighbour noise: CPU busy on the CPUs outside our cpuset (fraction of
    those CPUs) above max(--max-other, group median + --spike). The relative part is there
    because the metric rises with our own load: when our cpuset is saturated, the scheduler
    moves neighbours' unpinned tasks onto the other CPUs (A3: up to ~60 % on rows that use all
    8 CPUs, steady within a group). A fixed threshold alone rejected whole heavy groups, and
    rejected the busier client more often than the other. The relative test still drops
    neighbour bursts.
"""

import argparse
import fnmatch
import gzip
import json
import os
import statistics as st
import sys
from collections import defaultdict

METRICS = ("wall_s", "cpu_s", "maxrss_mb")


def read_lines(path):
    """Lines of a results file. Finished results are stored gzipped (pack-results.sh), live runs
    append to the plain file: `x.jsonl` and `x.jsonl.gz` name the same results, and both parts
    are read when both exist (the packed part first)."""
    base = path[:-3] if path.endswith(".gz") else path
    parts = [p for p in (base + ".gz", base) if os.path.exists(p)]
    if not parts:
        raise FileNotFoundError(path)
    for p in parts:
        with (gzip.open(p, "rt") if p.endswith(".gz") else open(p)) as f:
            yield from f


def load(paths):
    recs, seen = [], set()
    for p in paths:
        base = p[:-3] if p.endswith(".gz") else p
        if base in seen:
            continue
        seen.add(base)
        for line in read_lines(p):
            line = line.strip()
            if line:
                recs.append(json.loads(line))
    return recs


def ncpus(spec):
    n = 0
    for part in spec.split(","):
        if "-" in part:
            a, b = part.split("-")
            n += int(b) - int(a) + 1
        elif part:
            n += 1
    return n


def other_frac(r, n_other):
    cs = r.get("cpusets", {}).get("other")
    if not cs or r["wall_s"] <= 0:
        return 0.0
    return cs["busy_s"] / (r["wall_s"] * n_other)


def variant_of(r):
    v = r.get("variant") or ""
    if r.get("latency_ms"):
        v += f"lat{r['latency_ms']:g}ms"
    if r.get("fault_rate"):
        v += f"fault{r['fault_rate']:g}"
    if r.get("stdout") == "pipe":
        v += "pipe"
    return v


def summarize(recs, max_other, n_other, spike=0.20):
    groups = defaultdict(list)
    rejected = defaultdict(int)
    failed = defaultdict(int)
    level = defaultdict(list)
    for r in recs:
        if not r.get("warmup") and r.get("ok"):
            level[(r["row"], r["endpoint"], variant_of(r), r["client"])].append(
                other_frac(r, n_other))
    level = {k: st.median(v) for k, v in level.items()}
    for r in recs:
        if r.get("warmup"):
            continue
        key = (r["row"], r["endpoint"], variant_of(r), r["client"])
        if not r.get("ok"):
            failed[key] += 1
            continue
        if other_frac(r, n_other) > max(max_other, level[key] + spike):
            rejected[key] += 1
            continue
        r = dict(r)
        r["cpu_s"] = r["user_s"] + r["sys_s"]
        r["maxrss_mb"] = r["maxrss_kb"] / 1024
        groups[key].append(r)
    out = {}
    for key in set(groups) | set(failed) | set(rejected):
        rs = groups.get(key, [])
        s = {"n": len(rs), "rejected": rejected[key], "failed": failed[key], "rounds": {}}
        if rs:
            for m in METRICS:
                v = [r[m] for r in rs]
                s[m] = {"best": min(v), "median": st.median(v), "min": min(v), "max": max(v)}
            s["user_s"] = st.median(r["user_s"] for r in rs)
            s["sys_s"] = st.median(r["sys_s"] for r in rs)
            s["stub_cpu_s"] = st.median(r.get("stub_cpu_s") or 0 for r in rs)
            for grp in ("client", "server"):
                s[f"softirq_{grp}_s"] = st.median(
                    r.get("cpusets", {}).get(grp, {}).get("softirq_s", 0) for r in rs)
            s["rounds"] = {r["round"]: r for r in rs}
        out[key] = s
    return out


def ratio(a, b, m):
    """Median over shared rounds of a[m]/b[m]."""
    common = sorted(set(a["rounds"]) & set(b["rounds"]))
    vals = [a["rounds"][k][m] / b["rounds"][k][m] for k in common if b["rounds"][k][m] > 0]
    return st.median(vals) if vals else None


def verdict(a, b, m):
    """lower is better for every metric."""
    if not a.get(m) or not b.get(m):
        return "n/a"
    if a[m]["max"] < b[m]["min"]:
        return "win"
    if a[m]["min"] > b[m]["max"]:
        return "lose"
    return "tie"


def fmt(x, nd=2):
    if x is None:
        return "–"
    if abs(x) >= 100:
        return f"{x:.0f}"
    if abs(x) >= 10:
        return f"{x:.1f}"
    return f"{x:.{nd}f}"


def table(summary, baseline):
    rows = defaultdict(dict)
    for (row, ep, var, client), s in summary.items():
        rows[(row, ep, var)][client] = s
    lines = []
    hdr = ("| row | endpoint | client | n | wall best / med (min–max) s | CPU med (min–max) s | "
           "user / sys s | RSS med / max MB | wall× | CPU× | RSS× | verdict wall/CPU/RSS |")
    lines.append(hdr)
    lines.append("|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---|")
    for (row, ep, var) in sorted(rows):
        cl = rows[(row, ep, var)]
        base = cl.get(baseline)
        order = sorted(cl, key=lambda c: (c != baseline, c))
        for c in order:
            s = cl[c]
            label = f"{row}{(' ' + var) if var else ''}"
            if not s.get("n"):
                lines.append(f"| {label} | {ep} | {c} | 0 (failed {s['failed']}, rejected "
                             f"{s['rejected']}) | | | | | | | | |")
                continue
            w, cpu, rss = s["wall_s"], s["cpu_s"], s["maxrss_mb"]
            n = f"{s['n']}" + (f" (+{s['rejected']} rej)" if s["rejected"] else "") + \
                (f" ({s['failed']} FAILED)" if s["failed"] else "")
            if base and c != baseline and base.get("n"):
                rw, rc, rr = (ratio(s, base, m) for m in METRICS)
                v = "/".join(verdict(s, base, m) for m in METRICS)
            else:
                rw = rc = rr = None
                v = "baseline" if c == baseline else ""
            lines.append(
                f"| {label} | {ep} | {c} | {n} | {fmt(w['best'], 3)} / {fmt(w['median'], 3)} "
                f"({fmt(w['min'], 3)}–{fmt(w['max'], 3)}) | {fmt(cpu['median'])} "
                f"({fmt(cpu['min'])}–{fmt(cpu['max'])}) | {fmt(s['user_s'])} / {fmt(s['sys_s'])} | "
                f"{fmt(rss['median'], 1)} / {fmt(rss['max'], 1)} | {fmt(rw)} | {fmt(rc)} | {fmt(rr)} | {v} |")
    return "\n".join(lines)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("files", nargs="+")
    ap.add_argument("--max-other", type=float, default=0.35,
                    help="reject a run when the CPUs outside the cpuset were busier than this "
                         "(A2 pilot: quiet runs 4–19 %%, p90 11 %%) and than --spike over "
                         "the group's own median")
    ap.add_argument("--spike", type=float, default=0.20,
                    help="see --max-other; 1.0 turns the relative test off")
    ap.add_argument("--other-cpus", default="0-3,8-11")
    ap.add_argument("--rows", default="*")
    ap.add_argument("--endpoint", default="*")
    ap.add_argument("--baseline", default="s5cmd")
    ap.add_argument("--format", default="md", choices=("md", "json"))
    a = ap.parse_args()
    recs = [r for r in load(a.files)
            if any(fnmatch.fnmatch(r["row"], p) for p in a.rows.split(","))
            and fnmatch.fnmatch(r["endpoint"], a.endpoint)]
    summary = summarize(recs, a.max_other, ncpus(a.other_cpus), a.spike)
    if a.format == "json":
        out = {"|".join(k): {kk: vv for kk, vv in v.items() if kk != "rounds"}
               for k, v in summary.items()}
        json.dump(out, sys.stdout, indent=1, sort_keys=True)
        print()
        return
    print(table(summary, a.baseline))


if __name__ == "__main__":
    main()
