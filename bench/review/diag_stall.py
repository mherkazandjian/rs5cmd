"""Item 11 diagnostic: the io_uring + TLS stall (`--fast` runs over TLS that take ~0.9 s longer,
in about a third of the rounds). Starts the stub like the harness, writes 10k x 4 KiB files on
tmpfs, uploads them N times with `<bin> --fast` over stub-tls (pinned to the client CPUs, debug
log of the fast path on), and prints each run's wall time, its retry lines and their errors.
Run inside the bench container, inside a D1 window (io_uring), as one batch."""
import collections, os, re, shutil, subprocess, sys, time
sys.path.insert(0, os.path.dirname(__file__))
import matrix as mx

binary = sys.argv[1] if len(sys.argv) > 1 else f"{mx.BINS}/b-fast"
rounds = int(sys.argv[2]) if len(sys.argv) > 2 else 10
d = f"{mx.TMPFS}/diag-stall"
os.makedirs(d, exist_ok=True)
for i in range(10_000):
    with open(f"{d}/obj_{i:07d}.dat", "wb") as f:
        f.write(b"x" * 4096)
stub = mx.Stub(mx.CPU_SERVER).start()
env = {"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": "/tmp/clienthome", "AWS_REGION": "us-east-1",
       "AWS_ACCESS_KEY_ID": "minioadmin", "AWS_SECRET_ACCESS_KEY": "minioadmin",
       "SSL_CERT_FILE": f"{mx.CERTS}/ca.pem", "RUST_LOG": "rs5cmd::fastpath=debug"}
try:
    for r in range(rounds):
        t = time.monotonic()
        p = subprocess.run(["taskset", "-c", mx.CPU_CLIENT, binary, "--endpoint-url", mx.ENDPOINTS["stub-tls"],
                            "--no-verify-ssl", "cp", "--fast", f"{d}/*", f"s3://{mx.BUCKET}/diag-stall/"],
                           env=env, capture_output=True, text=True)
        wall = time.monotonic() - t
        retries = [l for l in p.stderr.splitlines() if "fastpath retry" in l]
        kinds = collections.Counter(re.sub(r".*after error: ", "", l)[:90] for l in retries)
        print(f"round {r}: exit {p.returncode} wall {wall:.2f}s retries {len(retries)} {dict(kinds)}")
finally:
    stub.stop()
    shutil.rmtree(d)
