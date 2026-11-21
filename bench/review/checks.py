#!/usr/bin/env python3
"""A4 small checks. Run inside bench after the measured batches, never during one:

    python3 bench/review/checks.py [c5] [f47] [sigterm] [creds]   # default: the first three; RS5_BIN=<rs5cmd binary>

c5       rs5cmd `cat` of a local 1 GiB file: peak RSS (audit C5: the whole file is read first).
f47      rs5cmd `cp --fast` under the client cpuset: which CPUs does it pin its shards to, and
         where do two concurrent runs land (audit F47)?
sigterm  SIGTERM one second into a slow upload (stub latency 1 s per request): time until exit
         and exit status, for s5cmd, rs5cmd and rs5cmd --fast.
creds    `ls` against the stub with credentials from each source rs5cmd resolves without a
         network service: environment, a credentials-file profile, a `credential_process`
         profile, and an SSO profile with no cached token (fails locally, before any request).
         Instance metadata is disabled. Prints exit status and stderr tail per source.

Prints one line per measurement; writes nothing to results/. Uses the stub on CPUSET_SERVER and
pins clients to CPUSET_CLIENT, like matrix.py. Temporary files live on the tmpfs and are removed.
"""

import os
import re
import shutil
import signal
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import matrix as mx  # noqa: E402

RS5 = os.environ.get("RS5_BIN", f"{mx.BINS}/rs5cmd")
S5 = f"{mx.BINS}/s5cmd"
URL = mx.ENDPOINTS["stub"]
ENV = {"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": "/tmp/clienthome",
       "AWS_ACCESS_KEY_ID": "minioadmin", "AWS_SECRET_ACCESS_KEY": "minioadmin",
       "AWS_REGION": "us-east-1"}
WORK = f"{mx.TMPFS}/checks"


def run_rusage(argv, timeout=120):
    """Run argv pinned to the client CPUs; return (exit, wall_s, maxrss_kb, stderr tail)."""
    t0 = time.time()
    p = subprocess.Popen(["taskset", "-c", mx.CPU_CLIENT] + argv, env=ENV,
                         stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    deadline = t0 + timeout
    while True:
        pid, status, ru = os.wait4(p.pid, os.WNOHANG)
        if pid:
            break
        if time.time() > deadline:
            p.kill()
            pid, status, ru = os.wait4(p.pid, 0)
            break
        time.sleep(0.01)
    p.returncode = os.waitstatus_to_exitcode(status)
    return p.returncode, time.time() - t0, ru.ru_maxrss, p.stderr.read()[-300:].decode(errors="replace")


def c5():
    os.makedirs(WORK, exist_ok=True)
    f = f"{WORK}/c5-1g.bin"
    with open(f, "wb") as out:
        chunk = os.urandom(1 << 20)
        for _ in range(1024):
            out.write(chunk)
    try:
        rc, wall, rss, err = run_rusage([RS5, "cat", f])
        print(f"c5   cat <local 1 GiB file>                 exit {rc} wall {wall:6.2f}s "
              f"maxrss {rss / 1024:7.1f} MB {err.strip()[:120]!r}")
    finally:
        os.unlink(f)


def f47():
    stub = mx.Stub(mx.CPU_SERVER).start()
    out = f"{WORK}/f47"
    try:
        stub.dataset("f47/", 2000, 4096)
        for n in (1, 2):
            procs, traces = [], []
            for i in range(n):
                d, tr = f"{out}-{i}", f"{WORK}/f47-{i}.strace"
                os.makedirs(d, exist_ok=True)
                traces.append(tr)
                procs.append(subprocess.Popen(
                    ["strace", "-f", "-qq", "-e", "trace=sched_setaffinity", "-o", tr,
                     "taskset", "-c", mx.CPU_CLIENT, RS5, "--endpoint-url", URL,
                     "cp", "--fast", "s3://bench/f47/*", d + "/"],
                    env=ENV, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE))
            for i, p in enumerate(procs):
                p.wait(120)
                cpus = []
                for line in open(traces[i]):
                    m = re.search(r"sched_setaffinity\(0, \d+, \[([\d ,-]+)\]\)\s+= 0", line)
                    if m and " " not in m.group(1):  # one CPU per shard thread
                        cpus.append(m.group(1))
                print(f"f47  {n} concurrent run(s), run {i}: exit {p.returncode}, "
                      f"threads pinned to CPUs {cpus} (process mask {mx.CPU_CLIENT}; taskset's own call excluded)")
            for i in range(n):
                shutil.rmtree(f"{out}-{i}", ignore_errors=True)
                os.unlink(traces[i])
    finally:
        stub.stop()


def sigterm():
    src = f"{WORK}/sig-src"
    os.makedirs(src, exist_ok=True)
    for i in range(64):
        with open(f"{src}/f{i:03d}", "wb") as f:
            f.write(b"x" * 4096)
    stub = mx.Stub(mx.CPU_SERVER, {"STUB_LATENCY_MS": "1000"}).start()
    try:
        cases = {
            "s5cmd": [S5, "--endpoint-url", URL, "--numworkers", "4", "cp", f"{src}/*", "s3://bench/sig/"],
            "rs5cmd": [RS5, "--endpoint-url", URL, "--numworkers", "4", "cp", f"{src}/*", "s3://bench/sig/"],
            "rs5cmd --fast": [RS5, "--endpoint-url", URL, "--numworkers", "4", "cp", "--fast",
                              f"{src}/*", "s3://bench/sig/"],
        }
        for name, argv in cases.items():
            t0 = time.time()
            p = subprocess.Popen(["taskset", "-c", mx.CPU_CLIENT] + argv, env=ENV,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            time.sleep(1.0)
            alive = p.poll() is None
            p.send_signal(signal.SIGTERM)
            t1 = time.time()
            try:
                p.wait(60)
                how = f"exit {p.returncode} {time.time() - t1:5.2f}s after SIGTERM"
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait()
                how = "still running 60 s after SIGTERM, killed"
            err = p.stderr.read()[-120:].decode(errors="replace").strip()
            print(f"sig  {name:<14} running at signal: {alive}; {how}; total {time.time() - t0:5.1f}s; "
                  f"stderr tail {err!r}")
    finally:
        stub.stop()
        shutil.rmtree(src, ignore_errors=True)


def creds():
    home = f"{WORK}/creds-home"
    os.makedirs(f"{home}/.aws", exist_ok=True)
    with open(f"{home}/.aws/credentials", "w") as f:
        f.write("[keys]\naws_access_key_id = minioadmin\naws_secret_access_key = minioadmin\n")
    # aws-config runs credential_process through `sh -c`.
    proc = "echo '{\"Version\":1,\"AccessKeyId\":\"minioadmin\",\"SecretAccessKey\":\"minioadmin\"}'"
    with open(f"{home}/.aws/config", "w") as f:
        f.write("[profile keys]\nregion = us-east-1\n"
                f"[profile process]\nregion = us-east-1\ncredential_process = {proc}\n"
                "[profile sso]\nregion = us-east-1\nsso_session = s\nsso_account_id = 111122223333\n"
                "sso_role_name = r\n[sso-session s]\nsso_start_url = https://example.invalid/start\n"
                "sso_region = us-east-1\n")
    base = {k: v for k, v in ENV.items() if not k.startswith("AWS_ACCESS") and not k.startswith("AWS_SECRET")}
    base.update(HOME=home, AWS_EC2_METADATA_DISABLED="true")
    stub = mx.Stub(mx.CPU_SERVER).start()
    try:
        cases = {"env": dict(ENV, HOME=home, AWS_EC2_METADATA_DISABLED="true"),
                 "profile": dict(base, AWS_PROFILE="keys"),
                 "process": dict(base, AWS_PROFILE="process"),
                 "sso": dict(base, AWS_PROFILE="sso")}
        for name, env in cases.items():
            p = subprocess.run(["taskset", "-c", mx.CPU_CLIENT, RS5, "--endpoint-url", URL, "ls", "s3://bench/"],
                               env=env, capture_output=True, text=True, timeout=60)
            err = " ".join(p.stderr.split())[-160:]
            print(f"cred {os.path.basename(RS5):<14} {name:<8} exit {p.returncode} stderr tail {err!r}")
    finally:
        stub.stop()
        shutil.rmtree(home, ignore_errors=True)


if __name__ == "__main__":
    os.makedirs(ENV["HOME"], exist_ok=True)
    os.makedirs(WORK, exist_ok=True)
    try:
        for c in sys.argv[1:] or ["c5", "f47", "sigterm"]:
            {"c5": c5, "f47": f47, "sigterm": sigterm, "creds": creds}[c]()
    finally:
        shutil.rmtree(WORK, ignore_errors=True)
