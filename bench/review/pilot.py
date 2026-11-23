#!/usr/bin/env python3
"""A2 pilot analysis: stub sensitivity, run-to-run spread, neighbour noise.

    pilot.py results/pilot-stubcpus.jsonl [more.jsonl ...]

1. Stub sensitivity: for each (row, client), wall/CPU medians and ranges per variant (e.g. the
   stub on 1 vs 2 physical cores), the ratio of medians, and how busy the stub's CPUs were. A
   client is stub-bound if giving the stub more cores moves its wall time beyond the spread.
2. Spread: (max - min) / median of wall and CPU per group, to choose the number of rounds.
3. Neighbour noise: the busy fraction of the CPUs outside the cpuset during each run.
"""
import json
import statistics as st
import sys
from collections import defaultdict

import aggregate as ag


def ncpus(spec):
    n = 0
    for part in (spec or "").split(","):
        if "-" in part:
            a, b = part.split("-")
            n += int(b) - int(a) + 1
        elif part:
            n += 1
    return n


def load(paths):
    out = []
    for p in paths:
        out += [json.loads(l) for l in ag.read_lines(p) if l.strip()]
    return [r for r in out if not r.get("warmup") and r.get("ok")]


def busy_frac(r, name, n):
    cs = r.get("cpusets", {}).get(name)
    if not cs or not n or r["wall_s"] <= 0:
        return None
    return cs["busy_s"] / (r["wall_s"] * n)


def main():
    recs = load(sys.argv[1:])
    g = defaultdict(list)
    for r in recs:
        r["cpu_s"] = r["user_s"] + r["sys_s"]
        g[(r["row"], r["client"], r.get("variant") or "")].append(r)

    print("## 1. Stub sensitivity (median wall s [min–max]; stub CPU busy %)\n")
    variants = sorted({k[2] for k in g})
    print("| row | client | " + " | ".join(variants) + " | wall ratio " +
          " / ".join(variants) + " | ranges overlap |")
    print("|---|---|" + "---:|" * len(variants) + "---:|---|")
    for row, client in sorted({(k[0], k[1]) for k in g}):
        cells, meds, ranges = [], [], []
        for v in variants:
            rs = g.get((row, client, v), [])
            if not rs:
                cells.append("–")
                meds.append(None)
                ranges.append(None)
                continue
            w = [r["wall_s"] for r in rs]
            sb = [busy_frac(r, "server", ncpus(r.get("stub_cpus") or r.get("cpuset_server")))
                  for r in rs]
            sb = [x for x in sb if x is not None]
            sbs = f"; stub {100 * st.median(sb):.0f}%" if sb else ""
            cells.append(f"{st.median(w):.3f} [{min(w):.3f}–{max(w):.3f}]{sbs}")
            meds.append(st.median(w))
            ranges.append((min(w), max(w)))
        rat = " / ".join(f"{m / meds[0]:.2f}" if m and meds[0] else "–" for m in meds)
        ov = "–"
        if all(ranges) and len(ranges) == 2:
            (a0, a1), (b0, b1) = ranges
            ov = "yes" if a0 <= b1 and b0 <= a1 else "NO"
        print(f"| {row} | {client} | " + " | ".join(cells) + f" | {rat} | {ov} |")

    print("\n## 2. Run-to-run spread, (max-min)/median\n")
    print("| row | client | variant | n | wall spread | CPU spread | RSS spread |")
    print("|---|---|---|---:|---:|---:|---:|")
    spreads = []
    for (row, client, v), rs in sorted(g.items()):
        if len(rs) < 2:
            continue
        def spr(m):
            x = [r[m] for r in rs]
            md = st.median(x)
            return (max(x) - min(x)) / md if md else 0.0
        sw, sc, sr = spr("wall_s"), spr("cpu_s"), spr("maxrss_kb")
        spreads.append(sw)
        print(f"| {row} | {client} | {v} | {len(rs)} | {100 * sw:.1f}% | {100 * sc:.1f}% | "
              f"{100 * sr:.1f}% |")
    if spreads:
        print(f"\nwall spread: median {100 * st.median(spreads):.1f}%, "
              f"max {100 * max(spreads):.1f}% over {len(spreads)} groups")

    print("\n## 3. Neighbour noise (busy fraction of CPUs outside the cpuset, per run)\n")
    fr = []
    for r in recs:
        n = ncpus(r.get("cpuset_other") or "0-3,8-11")
        f = busy_frac(r, "other", n)
        if f is not None:
            fr.append((f, r))
    if fr:
        xs = sorted(f for f, _ in fr)
        q = lambda p: xs[min(len(xs) - 1, int(p * len(xs)))]
        print(f"runs {len(xs)}: min {100 * xs[0]:.1f}%  p50 {100 * q(0.5):.1f}%  "
              f"p90 {100 * q(0.9):.1f}%  max {100 * xs[-1]:.1f}%")
        hi = sorted(fr, key=lambda t: -t[0])[:5]
        for f, r in hi:
            print(f"  {100 * f:.1f}%  {r['row']} {r['client']} {r.get('variant') or ''} "
                  f"r{r['round']} wall {r['wall_s']:.3f}s")


if __name__ == "__main__":
    main()
