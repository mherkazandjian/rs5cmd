"""D6 diagnostic: where large uploads spend the CPU that CRC32C adds. Starts the stub like the
harness, writes 4 x 1 GiB on tmpfs, runs `perf record -g` on one `cp` of them with <binary>
to [endpoint] (default stub; stub-tls: --no-verify-ssl)
(built with frame pointers and line tables), prints the top symbols (self time) and removes the
files. Run inside the bench container, never during a measurement batch."""
import os, shutil, subprocess, sys
sys.path.insert(0, os.path.dirname(__file__))
import matrix as mx

binary = sys.argv[1]
endpoint = sys.argv[2] if len(sys.argv) > 2 else "stub"
d = f"{mx.TMPFS}/diag-crc"
os.makedirs(d, exist_ok=True)
chunk = os.urandom(1 << 20)
for i in range(4):
    with open(f"{d}/f{i}.bin", "wb") as f:
        for _ in range(1024):
            f.write(chunk)
stub = mx.Stub(mx.CPU_SERVER).start()
env = {"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": "/tmp/clienthome", "AWS_REGION": "us-east-1",
       "AWS_ACCESS_KEY_ID": "minioadmin", "AWS_SECRET_ACCESS_KEY": "minioadmin"}
data = f"{mx.TMPFS}/diag-crc.perf"
try:
    p = subprocess.run(["perf", "record", "-F", "1999", "-g", "-o", data, "--", "taskset", "-c", mx.CPU_CLIENT,
                        binary, "--endpoint-url", mx.ENDPOINTS[endpoint], "--no-verify-ssl", "cp", f"{d}/*", f"s3://{mx.BUCKET}/diag-crc/"],
                       env=env, capture_output=True, text=True)
    print("exit", p.returncode, p.stderr.strip().splitlines()[-1:] if p.stderr.strip() else "")
    r = subprocess.run(["perf", "report", "-i", data, "--no-children", "--sort", os.environ.get("SORT", "symbol"), "--stdio",
                        "--percent-limit", "0.5", "-g", "none", "-w", "8,90"], capture_output=True, text=True)
    print("\n".join(l for l in r.stdout.splitlines() if l.strip() and not l.startswith("#"))[:6000])
finally:
    stub.stop()
    shutil.rmtree(d, ignore_errors=True)
    if os.path.exists(data):
        os.remove(data)
