"""Item 11 diagnostic, step 2: diag_stall.py's TLS --fast upload under strace -f -T (blocking
calls only); for each round prints the wall time and every call that blocked over 0.3 s."""
import os, re, shutil, subprocess, sys, time
sys.path.insert(0, os.path.dirname(__file__))
import matrix as mx

binary, rounds = sys.argv[1], int(sys.argv[2])
d = f"{mx.TMPFS}/diag-stall"
os.makedirs(d, exist_ok=True)
for i in range(10_000):
    with open(f"{d}/obj_{i:07d}.dat", "wb") as f:
        f.write(b"x" * 4096)
stub = mx.Stub(mx.CPU_SERVER).start()
env = {"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": "/tmp/clienthome", "AWS_REGION": "us-east-1",
       "AWS_ACCESS_KEY_ID": "minioadmin", "AWS_SECRET_ACCESS_KEY": "minioadmin"}
out = f"{mx.TMPFS}/diag-stall.strace"
try:
    for r in range(rounds):
        t = time.monotonic()
        p = subprocess.run(["taskset", "-c", mx.CPU_CLIENT, "strace", "-f", "-tt", "-T", "-o", out,
                            "-e", "trace=io_uring_enter,epoll_wait,epoll_pwait,futex,clock_nanosleep,nanosleep,poll,ppoll,read,exit_group,exit,clone,clone3,close",
                            binary, "--endpoint-url", mx.ENDPOINTS["stub-tls"], "--no-verify-ssl",
                            "cp", "--fast", f"{d}/*", f"s3://{mx.BUCKET}/diag-stall/"],
                           env=env, capture_output=True, text=True)
        wall = time.monotonic() - t
        long = []
        for line in open(out):
            m = re.search(r"<(\d+\.\d+)>$", line.strip())
            if m and float(m.group(1)) > 0.3:
                long.append(line.strip()[:200])
        print(f"round {r}: exit {p.returncode} wall {wall:.2f}s, {len(long)} calls > 0.3 s")
        for l in long[:8]:
            print("   ", l)
        if r == rounds - 1:
            first, last, lastcall, calls = {}, {}, {}, {}
            for line in open(out):
                parts = line.split(None, 2)
                if len(parts) < 3:
                    continue
                tid, ts, rest = parts
                first.setdefault(tid, ts)
                last[tid] = ts
                lastcall[tid] = rest.strip()[:110]
                calls[tid] = calls.get(tid, 0) + 1
            for tid in sorted(first, key=lambda t: first[t]):
                print(f"   tid {tid}: {first[tid]} -> {last[tid]}  {calls[tid]:6} calls  last: {lastcall[tid]}")
finally:
    stub.stop()
    shutil.rmtree(d)
    if os.path.exists(out):
        os.remove(out)
