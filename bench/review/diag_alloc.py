"""Item 13 diagnostic: where mimalloc v3 holds memory on a large upload (W3.up.4x1g shape).
Starts the stub like the harness, writes 4 x 1 GiB on tmpfs, uploads it once with b-stage (glibc)
and once with exp-mimalloc (MIMALLOC_ALLOW_THP=0, MIMALLOC_SHOW_STATS=1), prints peak RSS and
mimalloc's stats, then removes the files. Run inside the bench container."""
import os, shutil, subprocess, sys
sys.path.insert(0, os.path.dirname(__file__))
import matrix as mx

d = f"{mx.TMPFS}/diag-alloc"
os.makedirs(d, exist_ok=True)
for i in range(4):
    p = f"{d}/f{i}.dat"
    if not os.path.exists(p):
        subprocess.run(["head", "-c", str(mx.GiB), "/dev/urandom"], stdout=open(p, "wb"), check=True)
stub = mx.Stub(mx.CPU_SERVER).start()
url = mx.ENDPOINTS["stub"]
base = {"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": "/tmp/clienthome", "AWS_REGION": "us-east-1",
        "AWS_ACCESS_KEY_ID": "minioadmin", "AWS_SECRET_ACCESS_KEY": "minioadmin"}
try:
    for name, b, extra in (("b-stage", "b-stage", {}),
                           ("mi3-thp0", "exp-mimalloc",
                            {"MIMALLOC_ALLOW_THP": "0", "MIMALLOC_SHOW_STATS": "1"})):
        r = subprocess.run(["taskset", "-c", mx.CPU_CLIENT, "/usr/bin/time", "-f", "TIME rss=%MkB wall=%e",
                            f"{mx.BINS}/{b}", "--endpoint-url", url, "cp", f"{d}/*", "s3://bench/diag-alloc/"],
                           env={**base, **extra}, capture_output=True, text=True)
        err = r.stderr.splitlines()
        print(f"== {name} exit={r.returncode}", *[l for l in err if l.startswith("TIME")])
        if extra:
            print("\n".join(l for l in err if not l.startswith("cp ") and not l.startswith("TIME")))
finally:
    stub.stop()
    shutil.rmtree(d)
