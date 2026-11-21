#!/usr/bin/env python3
"""E8, the SDK floor: what does one small GET or PUT cost through aws-sdk-s3, and what through a
lean hyper client with rs5cmd's own SigV4 signer (the --fast path's signer, on tokio)?

    python3 bench/review/e8.py [--count 10000] [--size 4096] [--inflight 256] [--rounds 10]

Runs inside bench against s3stub (plain HTTP), pinned like matrix.py (stub on CPUSET_SERVER,
client under the launcher on CPUSET_CLIENT), rounds interleaved with the mode order rotated.
Modes of bins/sdkfloor (src/bin/sdkfloor.rs):
  sdk            aws-sdk-s3 Client (path style, checksums WhenRequired, like rs5cmd)
  lean           hyper-util legacy Client, Full<Bytes> bodies, rs5cmd::fastpath::sign::Signer
  lean-unsigned  the same without signing (what SigV4 itself costs)
Each run is checked against the stub's request counter. Results: results/a5-e8.jsonl, in
matrix.py's record format (row E8.get / E8.put, client sdkfloor-<mode>), so aggregate.py and
report.py read them (--baseline sdkfloor-sdk).
"""

import argparse
import json
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import matrix as mx  # noqa: E402

MODES = ("sdk", "lean", "lean-unsigned")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--count", type=int, default=10_000)
    ap.add_argument("--size", type=int, default=4096)
    ap.add_argument("--inflight", type=int, default=256)
    ap.add_argument("--rounds", type=int, default=10)
    ap.add_argument("--ops", default="get,put")
    ap.add_argument("--out", default=f"{mx.RESULTS}/a5-e8.jsonl")
    a = ap.parse_args()
    os.makedirs(mx.TMPFS, exist_ok=True)
    stub = mx.Stub(mx.CPU_SERVER).start()
    results = open(a.out, "a")
    url = mx.ENDPOINTS["stub"]
    try:
        stub.dataset("floor/", a.count, a.size)
        for op in a.ops.split(","):
            for rnd in range(a.rounds + 1):
                warm = rnd == 0
                order = MODES[rnd % 3:] + MODES[:rnd % 3]
                for mode in order:
                    mx.wait_for_load()
                    jpath = f"{mx.TMPFS}/e8.json"
                    if os.path.exists(jpath):
                        os.unlink(jpath)
                    argv = [f"{mx.BINS}/sdkfloor", mode, op, url, mx.BUCKET, "floor/",
                            str(a.count), str(a.size), str(a.inflight)]
                    stub.reset()
                    s0 = stub.cpu_s()
                    t0 = time.time()
                    p = subprocess.run(
                        [mx.LAUNCH, "-c", mx.CPU_CLIENT, "-g", f"client={mx.CPU_CLIENT}",
                         "-g", f"server={mx.CPU_SERVER}", "-g", f"other={mx.CPU_OTHER}",
                         "-o", jpath, "-t", "300", "--"] + argv,
                        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
                    stats = stub.stats()
                    with open(jpath) as f:
                        rec = json.loads(f.read().strip().splitlines()[-1])
                    want = a.count
                    got = stats.get("reqs", 0)
                    ok = p.returncode == 0 and got == want
                    rec.update({
                        "ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(t0)), "tag": "a5-e8",
                        "row": f"E8.{op}", "workload": "E8", "endpoint": "stub",
                        "client": f"sdkfloor-{mode}", "round": rnd, "warmup": warm,
                        "stdout": "null", "argv": argv, "ok": ok,
                        "check": {"reqs": got, "want": want,
                                  **({"stderr": p.stderr[-400:].decode(errors="replace")}
                                     if not ok else {})},
                        "stub_cpu_s": round(stub.cpu_s() - s0, 2),
                        "stub": {k: stats[k] for k in ("conns", "conns_peak", "reqs") if k in stats},
                        "inflight": a.inflight, "size": a.size, "count": a.count,
                    })
                    results.write(json.dumps(rec) + "\n")
                    results.flush()
                    print(f"  E8.{op:<4} {mode:<14} r{rnd}{'w' if warm else ' '} wall "
                          f"{rec['wall_s']:7.3f}s cpu {rec['user_s'] + rec['sys_s']:6.2f}s rss "
                          f"{rec['maxrss_kb'] / 1024:6.1f}MB conns {stats.get('conns')} "
                          f"{'ok' if ok else 'FAIL ' + json.dumps(rec['check'])[:200]}", flush=True)
    finally:
        stub.stop()


if __name__ == "__main__":
    main()
