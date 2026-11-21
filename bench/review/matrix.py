#!/usr/bin/env python3
"""Benchmark matrix for the optimization review (runs inside the pinned `bench` container).

    python3 bench/review/matrix.py list
    python3 bench/review/matrix.py run --rows 'W1.*' --endpoint stub --clients s5cmd,rs5cmd \
        [--rounds N] [--warmup 1] [--tag NAME] [--stub-cpus 4,5,12,13] [--stdout null|pipe|file]

Every measured command runs under `launch` pinned to CPUSET_CLIENT; s3stub runs pinned to
CPUSET_SERVER (MinIO is pinned there by compose). Rounds are interleaved across clients, with the
client order rotated each round; round 0 is a warm-up that is recorded but flagged. Each run is
checked (stub counters, file counts and contents, listing line counts) and appended as one JSON
line to results/<tag>.jsonl. aggregate.py turns those lines into tables.
"""

import argparse
import fnmatch
import json
import os
import shutil
import subprocess
import sys
import time
import urllib.parse
import urllib.request

BINS = "/opt/bins"
LAUNCH = f"{BINS}/launch"
PROBE = f"{BINS}/iouring-probe"
STUB_BIN = f"{BINS}/s3stub"
TMPFS = "/mnt/tmpfs"
DISK = "/mnt/disk"
HERE = os.path.dirname(os.path.abspath(__file__))
RESULTS = os.path.join(HERE, "results")
CERTS = "/cache/certs"
CPU_SERVER = os.environ.get("CPUSET_SERVER", "4,5,12,13")
CPU_CLIENT = os.environ.get("CPUSET_CLIENT", "6,7,14,15")
CPU_OTHER = os.environ.get("CPUSET_OTHER", "0-3,8-11")
STUB_PORT, STUB_TLS_PORT, STUB_H2_PORT, STUB_PQ_PORT = 9100, 9443, 9444, 9445
ENDPOINTS = {
    "stub": f"http://127.0.0.1:{STUB_PORT}",
    "stub-tls": f"https://127.0.0.1:{STUB_TLS_PORT}",
    # The stub's HTTPS listener that also offers HTTP/2, as an HTTP/2 front end does (#14).
    "stub-h2": f"https://127.0.0.1:{STUB_H2_PORT}",
    # HTTPS (HTTP/1.1) on aws-lc-rs, which offers the post-quantum hybrid X25519MLKEM768 first;
    # stub-tls is on ring and has no post-quantum group (#25).
    "stub-pq": f"https://127.0.0.1:{STUB_PQ_PORT}",
    "minio": "http://minio:9000",
}
BUCKET = "bench"
KiB, MiB, GiB = 1 << 10, 1 << 20, 1 << 30
# Local upload corpora for sync are backdated to OLD so the stub's copies (2024-01-01) are
# newer. Remote dates must not lie in the future: s5cmd leaves such objects out of listings.
OLD = 1_672_531_200  # 2023-01-01

# ------------------------------------------------------------------------------------------------
# Pattern content: byte o of every object/file is o % 251 (the stub serves the same pattern).
# ------------------------------------------------------------------------------------------------
PERIOD = 251
_PAT = bytes(i % PERIOD for i in range(MiB + PERIOD))


def pattern(off, n):
    """n pattern bytes starting at object offset off (n <= 1 MiB)."""
    a = off % PERIOD
    return _PAT[a:a + n]


def write_pattern(path, size):
    with open(path, "wb") as f:
        off = 0
        while off < size:
            n = min(MiB, size - off)
            f.write(pattern(off, n))
            off += n


def verify_pattern(path, size):
    try:
        if os.path.getsize(path) != size:
            return False
        with open(path, "rb") as f:
            off = 0
            while off < size:
                n = min(MiB, size - off)
                if f.read(n) != pattern(off, n):
                    return False
                off += n
        return True
    except OSError:
        return False


def name_of(i, width=7, name="obj_", suffix=".dat", fanout=0, dirs=""):
    """Same key template as s3stub datasets: [{i/fanout}/{dirs}]{name}{i}{suffix}."""
    head = f"{i // fanout:0{width}d}/{dirs}" if fanout else ""
    return f"{head}{name}{i:0{width}d}{suffix}"


def make_corpus(root, count, size, **tmpl):
    """count pattern files under root named like the stub's dataset keys; returns names."""
    shutil.rmtree(root, ignore_errors=True)
    os.makedirs(root)
    names = [name_of(i, **tmpl) for i in range(count)]
    small = pattern(0, size) if size <= MiB else None
    made = set()
    for n in names:
        p = os.path.join(root, n)
        d = os.path.dirname(p)
        if d not in made:
            os.makedirs(d, exist_ok=True)
            made.add(d)
        if small is not None:
            with open(p, "wb") as f:
                f.write(small)
        else:
            write_pattern(p, size)
    return names


def count_files(root):
    n = 0
    for _, _, files in os.walk(root):
        n += len(files)
    return n


def verify_tree(root, names, size, sizes=None):
    """All names present with pattern content; returns (ok, detail)."""
    bad = 0
    for n in names:
        if not verify_pattern(os.path.join(root, n), sizes.get(n, size) if sizes else size):
            bad += 1
    extra = count_files(root) - len(names)
    return bad == 0 and extra == 0, {"bad_files": bad, "extra_files": extra}


def wipe(path):
    shutil.rmtree(path, ignore_errors=True)
    os.makedirs(path, exist_ok=True)


# ------------------------------------------------------------------------------------------------
# s3stub control
# ------------------------------------------------------------------------------------------------
class Stub:
    def __init__(self, cpus, env_extra=None):
        self.cpus = cpus
        self.env_extra = env_extra or {}
        self.proc = None

    def start(self):
        ensure_certs()
        env = dict(os.environ)
        env.update({
            "STUB_PORT": str(STUB_PORT), "STUB_TLS_PORT": str(STUB_TLS_PORT),
            "STUB_H2_PORT": str(STUB_H2_PORT), "STUB_TLS_PQ_PORT": str(STUB_PQ_PORT),
            "STUB_TLS_CERT": f"{CERTS}/server.pem", "STUB_TLS_KEY": f"{CERTS}/server.key",
        })
        env.update(self.env_extra)
        log = open(f"{TMPFS}/s3stub.log", "ab")
        self.proc = subprocess.Popen(["taskset", "-c", self.cpus, STUB_BIN], env=env,
                                     stdout=subprocess.DEVNULL, stderr=log)
        for _ in range(100):
            try:
                self.stats()
                return self
            except OSError:
                time.sleep(0.05)
        raise RuntimeError("s3stub did not come up")

    def stop(self):
        if self.proc:
            self.proc.terminate()
            self.proc.wait(10)
            self.proc = None

    def ctl(self, method, what, **q):
        url = f"http://127.0.0.1:{STUB_PORT}/__stub/{what}"
        if q:
            url += "?" + urllib.parse.urlencode(q)
        req = urllib.request.Request(url, method=method, data=b"" if method != "GET" else None)
        with urllib.request.urlopen(req, timeout=10) as r:
            return json.loads(r.read() or b"{}")

    def stats(self):
        return self.ctl("GET", "stats")

    def reset(self):
        self.ctl("POST", "reset")

    def config(self, **q):
        return self.ctl("POST", "config", **q)

    def dataset(self, prefix, count, size, **kw):
        return self.ctl("PUT", "dataset", bucket=BUCKET, prefix=prefix, count=count, size=size, **kw)

    def clear(self):
        self.ctl("DELETE", "dataset")

    def cpu_s(self):
        with open(f"/proc/{self.proc.pid}/stat") as f:
            parts = f.read().rsplit(")", 1)[1].split()
        hz = os.sysconf("SC_CLK_TCK")
        return (int(parts[11]) + int(parts[12])) / hz  # utime, stime


def ensure_certs():
    """A test CA and a server certificate for 127.0.0.1 / localhost (stub TLS listener)."""
    if os.path.exists(f"{CERTS}/server.pem"):
        return
    os.makedirs(CERTS, exist_ok=True)
    run = lambda *a: subprocess.run(a, check=True, capture_output=True, cwd=CERTS)
    run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "3650",
        "-keyout", "ca.key", "-out", "ca.pem", "-subj", "/CN=rs5cmd bench CA")
    run("openssl", "req", "-newkey", "rsa:2048", "-nodes", "-keyout", "server.key",
        "-out", "server.csr", "-subj", "/CN=localhost")
    with open(f"{CERTS}/ext.cnf", "w") as f:
        f.write("subjectAltName=IP:127.0.0.1,DNS:localhost\nbasicConstraints=CA:FALSE\n"
                "keyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n")
    run("openssl", "x509", "-req", "-in", "server.csr", "-CA", "ca.pem", "-CAkey", "ca.key",
        "-CAcreateserial", "-days", "3650", "-out", "server.pem", "-extfile", "ext.cnf")


# ------------------------------------------------------------------------------------------------
# Clients
# ------------------------------------------------------------------------------------------------
CLIENTS = {
    "s5cmd": {"bin": f"{BINS}/s5cmd"},
    "s5cmd-go122": {"bin": f"{BINS}/s5cmd-go122"},
    "s5cmd-golatest": {"bin": f"{BINS}/s5cmd-golatest"},
    # RS5CMD_BIN replaces the binary behind `rs5cmd` and `rs5cmd-fast` (Stage B re-baselines).
    "rs5cmd": {"bin": os.environ.get("RS5CMD_BIN", f"{BINS}/rs5cmd")},
    "rs5cmd-fast": {"bin": os.environ.get("RS5CMD_BIN", f"{BINS}/rs5cmd"), "fast": True},
    "rs5cmd-prof": {"bin": f"{BINS}/rs5cmd-prof"},
}


def is_rs(client):
    return client.startswith("rs5cmd")


def client_flags(flags, client):
    """A row's per-client flags. Extra rs5cmd clients (--client-bin rs5cmd-...) get the flags of
    `rs5cmd` or `rs5cmd-fast`, so a renamed build runs the same command line."""
    if client in flags:
        return flags[client]
    if is_rs(client):
        return flags.get("rs5cmd-fast" if CLIENTS[client].get("fast") else "rs5cmd", ())
    return ()


class Ctx:
    def __init__(self, args):
        self.args = args
        self.endpoint = args.endpoint
        self.url = ENDPOINTS[args.endpoint]
        self.stub = None
        self.extra_env = dict(kv.split("=", 1) for kv in args.env)

    def env(self, region=True):
        e = {"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": "/tmp/clienthome",
             "AWS_ACCESS_KEY_ID": "minioadmin", "AWS_SECRET_ACCESS_KEY": "minioadmin"}
        os.makedirs(e["HOME"], exist_ok=True)
        if region:
            e["AWS_REGION"] = "us-east-1"
        if self.endpoint in ("stub-tls", "stub-h2", "stub-pq"):
            e["SSL_CERT_FILE"] = f"{CERTS}/ca.pem"
        e.update(self.extra_env)
        return e

    def cli(self, client, sub, *args, cmd_flags=(), global_flags=()):
        c = CLIENTS[client]
        a = [c["bin"], "--endpoint-url", self.url]
        if c.get("fast") and self.endpoint in ("stub-tls", "stub-h2", "stub-pq"):
            a.append("--no-verify-ssl")  # the fast path has no custom-CA option
        a += c.get("global_flags", [])
        a += list(global_flags)
        a.append(sub)
        if c.get("fast") and sub in ("cp", "mv"):
            a.append("--fast")
        a += list(cmd_flags)
        a += list(args)
        return a

    def s3(self, path=""):
        return f"s3://{BUCKET}/{path}"

    def helper(self, *args, check=True, timeout=3600):
        """Untimed helper command via s5cmd (setup/cleanup on MinIO), unpinned from the client set."""
        a = [CLIENTS["s5cmd"]["bin"], "--endpoint-url", self.url, *args]
        r = subprocess.run(a, env=self.env(), capture_output=True, timeout=timeout)
        if check and r.returncode != 0:
            raise RuntimeError(f"helper failed: {' '.join(a)}: {r.stderr.decode()[-500:]}")
        return r


class Run:
    """One measured command: argv, optional stdin file, and whatever check() needs."""

    def __init__(self, argv, stdin=None, env=None, **info):
        self.argv, self.stdin, self.env, self.info = argv, stdin, env, info


# ------------------------------------------------------------------------------------------------
# Workloads. Each row: prepare (once), setup -> Run (per client and round), check, teardown,
# finish. `long` rows (>= 5 s) get 5 rounds, short ones 10 (plus the warm-up).
# ------------------------------------------------------------------------------------------------
class Row:
    long = False
    fast_ok = False  # --fast applies (cp/mv)
    endpoints = ("stub", "minio")
    workload = "?"

    def __init__(self, rid, **p):
        self.id, self.p = rid, p
        if "endpoints" in p:  # per-row override, e.g. rows too large for MinIO within the caps
            self.endpoints = p["endpoints"]

    def prepare(self, ctx):
        pass

    def setup(self, ctx, client, tag):
        raise NotImplementedError

    def check(self, ctx, client, tag, run, stats, out_path):
        return True, {}

    def teardown(self, ctx, client, tag):
        pass

    def finish(self, ctx):
        pass

    def lines_ok(self, out_path, want):
        if out_path is None:
            return None
        with open(out_path, "rb") as f:
            n = sum(1 for _ in f)
        return n == want


class Upload(Row):
    """cp <local dir>/* s3://bench/<fresh prefix>/ (W1, W2, W3 upload)."""
    fast_ok = True
    endpoints = ("stub", "stub-tls", "stub-pq", "minio")

    def prepare(self, ctx):
        p = self.p
        self.src = f"{p.get('fsroot', TMPFS)}/corpus-{self.id}"
        self.tmpl = p.get("tmpl", {})
        self.names = make_corpus(self.src, p["count"], p["size"], **self.tmpl)
        if ctx.endpoint == "minio":
            ctx.helper("mb", ctx.s3(), check=False)

    def setup(self, ctx, client, tag):
        self.prefix = f"up-{self.id}-{tag}/"
        return Run(ctx.cli(client, "cp", f"{self.src}/*", ctx.s3(self.prefix),
                           cmd_flags=client_flags(self.p.get("flags", {}), client),
                           global_flags=self.p.get("gflags", ())))

    def check(self, ctx, client, tag, run, stats, out_path):
        p = self.p
        want_bytes = p["count"] * p["size"]
        info = {"lines_ok": self.lines_ok(out_path, p["count"])}
        if stats is not None:
            objs = stats["puts"] + stats["mpu_complete"]
            got = stats["put_bytes"] + stats["mpu_part_bytes"]
            info.update(objects=objs, bytes=got)
            return objs == p["count"] and got == want_bytes and info["lines_ok"] is not False, info
        r = ctx.helper("du", ctx.s3(self.prefix + "*"), check=False)
        out = r.stdout.decode()
        info["du"] = out.strip()[-200:]
        ok = f"{want_bytes} bytes in {p['count']} objects" in out
        return ok and info["lines_ok"] is not False, info

    def teardown(self, ctx, client, tag):
        if ctx.endpoint == "minio":
            ctx.helper("rm", ctx.s3(self.prefix + "*"), check=False)

    def finish(self, ctx):
        shutil.rmtree(self.src, ignore_errors=True)


class Download(Row):
    """cp 's3://bench/<prefix>/*' <local dir>/ (W1, W2, W3 download)."""
    fast_ok = True
    endpoints = ("stub", "stub-tls", "stub-h2", "stub-pq", "minio")

    def prepare(self, ctx):
        p = self.p
        self.tmpl = p.get("tmpl", {})
        self.prefix = f"dn-{self.id}/"
        self.names = [name_of(i, **self.tmpl) for i in range(p["count"])]
        self.dst = f"{p.get('fsroot', TMPFS)}/out"
        if ctx.stub:
            ctx.stub.dataset(self.prefix, p["count"], p["size"], **self.tmpl)
        else:
            ctx.helper("mb", ctx.s3(), check=False)
            src = f"{TMPFS}/seed"
            make_corpus(src, p["count"], p["size"], **self.tmpl)
            ctx.helper("--numworkers", SEED_WORKERS, "cp", f"{src}/*", ctx.s3(self.prefix))
            shutil.rmtree(src, ignore_errors=True)

    def setup(self, ctx, client, tag):
        wipe(self.dst)
        return Run(ctx.cli(client, "cp", ctx.s3(self.prefix + "*"), self.dst + "/",
                           cmd_flags=client_flags(self.p.get("flags", {}), client),
                           global_flags=self.p.get("gflags", ())))

    def check(self, ctx, client, tag, run, stats, out_path):
        p = self.p
        ok, info = verify_tree(self.dst, self.names, p["size"])
        info["lines_ok"] = self.lines_ok(out_path, p["count"])
        if stats is not None:
            info["gets"] = stats["gets"]
        return ok and info["lines_ok"] is not False, info

    def teardown(self, ctx, client, tag):
        shutil.rmtree(self.dst, ignore_errors=True)

    def finish(self, ctx):
        if ctx.endpoint == "minio":
            ctx.helper("rm", ctx.s3(self.prefix + "*"), check=False)


class Ls(Row):
    """ls s3://bench/<prefix>/ (W4) and du (W10)."""
    endpoints = ("stub", "minio")

    def prepare(self, ctx):
        p = self.p
        self.prefix = f"ls-{p['count']}/"
        if ctx.stub:
            ctx.stub.dataset(self.prefix, p["count"], 4096)
        else:
            ctx.helper("mb", ctx.s3(), check=False)
            r = ctx.helper("du", ctx.s3(self.prefix + "*"), check=False)
            if f"in {p['count']} objects" not in r.stdout.decode():
                src = f"{TMPFS}/seed"
                make_corpus(src, p["count"], 4096)
                ctx.helper("--numworkers", SEED_WORKERS, "cp", f"{src}/*", ctx.s3(self.prefix))
                shutil.rmtree(src, ignore_errors=True)

    def setup(self, ctx, client, tag):
        if self.p.get("du"):
            return Run(ctx.cli(client, "du", ctx.s3(self.prefix + "*")))
        return Run(ctx.cli(client, "ls", ctx.s3(self.prefix)))

    def check(self, ctx, client, tag, run, stats, out_path):
        want = 1 if self.p.get("du") else self.p["count"]
        info = {"lines_ok": self.lines_ok(out_path, want)}
        if stats is not None:
            info["list_keys"] = stats["list_keys"]
            return stats["list_keys"] == self.p["count"] and info["lines_ok"] is not False, info
        return info["lines_ok"] is not False, info


class Rm(Row):
    """rm 's3://bench/<prefix>/*' (W6)."""
    endpoints = ("stub", "minio")

    def prepare(self, ctx):
        if ctx.stub:
            self.prefix = "rm/"
            ctx.stub.dataset(self.prefix, self.p["count"], 4096)
        else:
            ctx.helper("mb", ctx.s3(), check=False)
            self.seed = f"{TMPFS}/seed-rm"
            make_corpus(self.seed, self.p["count"], 4096)

    def setup(self, ctx, client, tag):
        if not ctx.stub:
            self.prefix = f"rm-{tag}/"
            ctx.helper("--numworkers", SEED_WORKERS, "cp", f"{self.seed}/*", ctx.s3(self.prefix))
        return Run(ctx.cli(client, "rm", ctx.s3(self.prefix + "*")))

    def check(self, ctx, client, tag, run, stats, out_path):
        n = self.p["count"]
        info = {"lines_ok": self.lines_ok(out_path, n)}
        if stats is not None:
            info["deleted"] = stats["delete_keys"] + stats["delete_one"]
            info["delete_requests"] = stats["delete_batches"] + stats["delete_one"]
            return info["deleted"] == n and info["lines_ok"] is not False, info
        r = ctx.helper("ls", ctx.s3(self.prefix + "*"), check=False)
        left = len(r.stdout.splitlines())
        info["left"] = left
        return left == 0 and info["lines_ok"] is not False, info

    def finish(self, ctx):
        if not ctx.stub:
            shutil.rmtree(self.seed, ignore_errors=True)


class Cat(Row):
    """cat s3://bench/<obj> > /dev/null (W7)."""
    long = True
    endpoints = ("stub", "minio")

    def prepare(self, ctx):
        self.key = "cat/obj_0000000.dat"
        if ctx.stub:
            ctx.stub.dataset("cat/", 1, self.p["size"])
        else:
            ctx.helper("mb", ctx.s3(), check=False)
            src = f"{TMPFS}/seed-cat"
            make_corpus(src, 1, self.p["size"])
            ctx.helper("cp", f"{src}/obj_0000000.dat", ctx.s3(self.key))
            shutil.rmtree(src, ignore_errors=True)

    def setup(self, ctx, client, tag):
        return Run(ctx.cli(client, "cat", ctx.s3(self.key)), stdout_check=True)

    def check(self, ctx, client, tag, run, stats, out_path):
        size = self.p["size"]
        info = {}
        if out_path is not None:
            info["content_ok"] = verify_pattern(out_path, size)
        if stats is not None:
            info["get_bytes"] = stats["get_bytes"]
            return stats["get_bytes"] >= size and info.get("content_ok", True), info
        return info.get("content_ok", True), info

    def finish(self, ctx):
        if not ctx.stub:
            ctx.helper("rm", ctx.s3(self.key), check=False)


class Pipe(Row):
    """pipe s3://bench/<key> < 4 GiB file (W7)."""
    long = True
    endpoints = ("stub", "minio")

    def prepare(self, ctx):
        self.src = f"{TMPFS}/pipe-src.bin"
        write_pattern(self.src, self.p["size"])
        if not ctx.stub:
            ctx.helper("mb", ctx.s3(), check=False)

    def setup(self, ctx, client, tag):
        self.key = f"pipe-{tag}/obj.dat"
        return Run(ctx.cli(client, "pipe", ctx.s3(self.key)), stdin=self.src)

    def check(self, ctx, client, tag, run, stats, out_path):
        size = self.p["size"]
        if stats is not None:
            got = stats["put_bytes"] + stats["mpu_part_bytes"]
            return got == size, {"bytes": got}
        r = ctx.helper("du", ctx.s3(self.key), check=False)
        return f"{size} bytes in 1 objects" in r.stdout.decode(), {"du": r.stdout.decode().strip()}

    def teardown(self, ctx, client, tag):
        if not ctx.stub:
            ctx.helper("rm", ctx.s3(self.key), check=False)

    def finish(self, ctx):
        os.unlink(self.src)


class RunFile(Row):
    """run <file of N cp lines> (W8), upload or download of 4 KiB objects."""

    def prepare(self, ctx):
        p = self.p
        self.n = p["count"]
        self.src = f"{TMPFS}/runsrc"
        self.dst = f"{TMPFS}/out"
        self.names = [name_of(i) for i in range(self.n)]
        if p["dir"] == "up":
            make_corpus(self.src, self.n, 4096)
            if not ctx.stub:
                ctx.helper("mb", ctx.s3(), check=False)
        else:
            self.prefix = "run-dn/"
            if ctx.stub:
                ctx.stub.dataset(self.prefix, self.n, 4096)
            else:
                ctx.helper("mb", ctx.s3(), check=False)
                make_corpus(self.src, self.n, 4096)
                ctx.helper("--numworkers", SEED_WORKERS, "cp", f"{self.src}/*", ctx.s3(self.prefix))
                shutil.rmtree(self.src, ignore_errors=True)

    def setup(self, ctx, client, tag):
        path = f"{TMPFS}/run-{tag}.txt"
        with open(path, "w") as f:
            if self.p["dir"] == "up":
                self.prefix = f"run-up-{tag}/"
                for n in self.names:
                    f.write(f"cp {self.src}/{n} {ctx.s3(self.prefix + n)}\n")
            else:
                wipe(self.dst)
                for n in self.names:
                    f.write(f"cp {ctx.s3(self.prefix + n)} {self.dst}/{n}\n")
        self.runfile = path
        return Run(ctx.cli(client, "run", path))

    def check(self, ctx, client, tag, run, stats, out_path):
        info = {"lines_ok": self.lines_ok(out_path, self.n)}
        if self.p["dir"] == "dn":
            ok, more = verify_tree(self.dst, self.names, 4096)
            info.update(more)
            return ok and info["lines_ok"] is not False, info
        if stats is not None:
            info["puts"] = stats["puts"]
            return stats["puts"] == self.n and info["lines_ok"] is not False, info
        r = ctx.helper("du", ctx.s3(self.prefix + "*"), check=False)
        return f"in {self.n} objects" in r.stdout.decode() and info["lines_ok"] is not False, info

    def teardown(self, ctx, client, tag):
        os.unlink(self.runfile)
        if self.p["dir"] == "up" and not ctx.stub:
            ctx.helper("rm", ctx.s3(self.prefix + "*"), check=False)

    def finish(self, ctx):
        shutil.rmtree(self.src, ignore_errors=True)
        shutil.rmtree(self.dst, ignore_errors=True)


class Tiny(Row):
    """Startup and one-object operations (W9), with and without a configured region."""
    endpoints = ("stub", "minio")

    def prepare(self, ctx):
        self.one = f"{TMPFS}/one.dat"
        write_pattern(self.one, 4096)
        if ctx.stub:
            ctx.stub.dataset("tiny/", 1, 4096)
        else:
            ctx.helper("mb", ctx.s3(), check=False)
            ctx.helper("cp", self.one, ctx.s3("tiny/obj_0000000.dat"))

    def setup(self, ctx, client, tag):
        env = ctx.env(region=self.p.get("region", True))
        op = self.p["op"]
        if op == "version":
            # rs5cmd has no `version` subcommand; `--version` is its equivalent.
            argv = [CLIENTS[client]["bin"], "--version" if is_rs(client) else "version"]
        elif op == "ls1":
            argv = ctx.cli(client, "ls", ctx.s3("tiny/obj_0000000.dat"))
        else:
            argv = ctx.cli(client, "cp", self.one, ctx.s3(f"tiny-up/{tag}.dat"))
        return Run(argv, env=env)

    def check(self, ctx, client, tag, run, stats, out_path):
        return True, {}


class SyncStub(Row):
    """sync against the stub (W5): the remote side is a synthetic dataset."""
    long = False
    endpoints = ("stub",)

    def prepare(self, ctx):
        p = self.p
        self.n = p["count"]
        self.names = [name_of(i) for i in range(self.n)]
        self.local = f"{TMPFS}/sync-local"
        make_corpus(self.local, self.n, 4096)
        self.prefix = f"sync-{self.id}/"
        mode, d = p["mode"], p["dir"]
        extra = self.n // 10
        if d == "up":
            for n in self.names:
                os.utime(os.path.join(self.local, n), (OLD, OLD))
            if mode == "cold":
                ctx.stub.ctl("DELETE", "dataset", bucket=BUCKET)
            elif mode == "warm":
                ctx.stub.dataset(self.prefix, self.n, 4096)
            elif mode == "changed":
                ctx.stub.dataset(self.prefix, self.n, 4096, change_mod=10,
                                 change_size=4097)
            elif mode == "delete":
                ctx.stub.dataset(self.prefix, self.n + extra, 4096)
        else:
            if mode == "changed":
                ctx.stub.dataset(self.prefix, self.n, 4096, change_mod=10, change_size=4097)
            else:
                ctx.stub.dataset(self.prefix, self.n, 4096)

    def setup(self, ctx, client, tag):
        p = self.p
        flags = ["--delete"] if p["mode"] == "delete" else []
        if p["dir"] == "up":
            return Run(ctx.cli(client, "sync", *flags, self.local + "/", ctx.s3(self.prefix)))
        mode = p["mode"]
        if mode == "cold":
            wipe(self.local)
            with open(os.path.join(self.local, ".keep"), "w"):
                pass  # an empty destination fails in rs5cmd (issue #4)
        elif mode == "changed":
            # Restore the 10 % the previous round re-downloaded at the changed size.
            for i in range(0, self.n, 10):
                write_pattern(os.path.join(self.local, self.names[i]), 4096)
        elif mode == "delete":
            for i in range(self.n, self.n + self.n // 10):
                write_pattern(os.path.join(self.local, name_of(i)), 4096)
        return Run(ctx.cli(client, "sync", *flags, ctx.s3(self.prefix + "*"), self.local + "/"))

    def check(self, ctx, client, tag, run, stats, out_path):
        p, n = self.p, self.n
        want = {
            ("up", "cold"): ("puts", n), ("up", "warm"): ("puts", 0),
            ("up", "changed"): ("puts", (n + 9) // 10), ("up", "delete"): ("deleted", n // 10),
            ("dn", "cold"): ("gets", n), ("dn", "warm"): ("gets", 0),
            ("dn", "changed"): ("gets", (n + 9) // 10), ("dn", "delete"): ("gets", 0),
        }[(p["dir"], p["mode"])]
        # rs5cmd's sync deletes with one DELETE per key, s5cmd with DeleteObjects batches.
        stats = dict(stats, deleted=stats["delete_keys"] + stats["delete_one"])
        info = {want[0]: stats[want[0]], "puts": stats["puts"], "gets": stats["gets"],
                "delete_requests": stats["delete_one"] + stats["delete_batches"]}
        ok = stats[want[0]] == want[1]
        if p["dir"] == "dn":
            info["local_files"] = count_files(self.local)
            ok = ok and info["local_files"] == n + (1 if p["mode"] == "cold" else 0)
        return ok, info

    def finish(self, ctx):
        shutil.rmtree(self.local, ignore_errors=True)


class SyncMinio(Row):
    """sync against MinIO (W5): one chain per client and round on a fresh prefix/dir:
    cold, warm, changed (10 % of the files differ in size), delete (10 % only on one side)."""
    endpoints = ("minio",)

    def prepare(self, ctx):
        self.n = self.p["count"]
        self.names = [name_of(i) for i in range(self.n)]
        self.seed = f"{TMPFS}/sync-seed"
        make_corpus(self.seed, self.n, 4096)
        ctx.helper("mb", ctx.s3(), check=False)
        if self.p["dir"] == "dn":
            self.remote = "sync-dn-src/"
            ctx.helper("--numworkers", SEED_WORKERS, "cp", f"{self.seed}/*", ctx.s3(self.remote))

    def chain(self):
        return ["cold", "warm", "changed", "delete"]

    def setup_step(self, ctx, client, tag, mode):
        n = self.n
        flags = ["--delete"] if mode == "delete" else []
        if self.p["dir"] == "up":
            local = self.seed
            if mode == "cold":
                self.prefix = f"sync-up-{tag}/"
            elif mode == "changed":
                for i in range(0, n, 10):
                    write_pattern(os.path.join(local, self.names[i]), 4097)
            elif mode == "delete":
                self.moved = f"{TMPFS}/sync-moved"
                wipe(self.moved)
                for i in range(0, n, 10):
                    os.rename(os.path.join(local, self.names[i]), os.path.join(self.moved, self.names[i]))
            return Run(ctx.cli(client, "sync", *flags, local + "/", ctx.s3(self.prefix)))
        self.local = f"{TMPFS}/sync-dst"
        if mode == "cold":
            wipe(self.local)
            with open(os.path.join(self.local, ".keep"), "w"):
                pass  # an empty destination fails in rs5cmd (issue #4)
        elif mode == "warm":
            os.unlink(os.path.join(self.local, ".keep"))
        elif mode == "changed":
            for i in range(0, n, 10):
                write_pattern(os.path.join(self.local, self.names[i]), 4000)
        elif mode == "delete":
            for i in range(n, n + n // 10):
                write_pattern(os.path.join(self.local, name_of(i)), 4096)
        return Run(ctx.cli(client, "sync", *flags, ctx.s3(self.remote + "*"), self.local + "/"))

    def check_step(self, ctx, client, tag, mode, out_path):
        n = self.n
        want_lines = {"cold": n, "warm": 0, "changed": (n + 9) // 10, "delete": n // 10}[mode]
        info = {"lines_ok": self.lines_ok(out_path, want_lines)}
        if self.p["dir"] == "dn":
            files = count_files(self.local) - (1 if mode == "cold" else 0)  # .keep
            info["local_files"] = files
            ok = files == n
            if mode in ("cold", "changed"):
                vok, more = verify_tree_subset(self.local, self.names, 4096)
                info.update(more)
                ok = ok and vok
            return ok and info["lines_ok"] is not False, info
        r = ctx.helper("du", ctx.s3(self.prefix + "*"), check=False)
        out = r.stdout.decode()
        want = {"cold": n, "warm": n, "changed": n, "delete": n - (n + 9) // 10}[mode]
        info["du"] = out.strip()[-120:]
        return f"in {want} objects" in out and info["lines_ok"] is not False, info

    def end_chain(self, ctx, client, tag):
        if self.p["dir"] == "up":
            for i in range(0, self.n, 10):
                src = os.path.join(self.moved, self.names[i])
                os.rename(src, os.path.join(self.seed, self.names[i]))
                write_pattern(os.path.join(self.seed, self.names[i]), 4096)
            ctx.helper("rm", ctx.s3(self.prefix + "*"), check=False)
        else:
            shutil.rmtree(self.local, ignore_errors=True)

    def finish(self, ctx):
        shutil.rmtree(self.seed, ignore_errors=True)
        if self.p["dir"] == "dn":
            ctx.helper("rm", ctx.s3(self.remote + "*"), check=False)


def verify_tree_subset(root, names, size):
    bad = sum(1 for n in names if not verify_pattern(os.path.join(root, n), size))
    return bad == 0, {"bad_files": bad}


# ------------------------------------------------------------------------------------------------
# Registry
# ------------------------------------------------------------------------------------------------
ROWS = {}


def reg(cls, rid, workload, long=None, **p):
    ROWS[rid] = (cls, workload, long, p)


STUB_ONLY = ("stub",)
DEEP = {"fanout": 1000, "dirs": "level-two/level-three/level-four/",
        "name": "a-reasonably-long-object-name-for-key-handling-"}
S5_EQ = {"rs5cmd": ("--concurrency", "5", "--part-size", "50"),
         "rs5cmd-fast": ("--concurrency", "5", "--part-size", "50")}
for n, tag in ((100_000, "100k"), (10_000, "10k")):
    reg(Upload, f"W1.up.{tag}", "W1", long=n >= 100_000, count=n, size=4 * KiB)
    reg(Download, f"W1.dn.{tag}", "W1", long=n >= 100_000, count=n, size=4 * KiB)
reg(Upload, "W1.up.deep", "W1", long=True, count=100_000, size=4 * KiB, tmpl=DEEP)
reg(Download, "W1.dn.deep", "W1", long=True, count=100_000, size=4 * KiB, tmpl=DEEP)
reg(Upload, "W1.up.1k", "W1", count=1000, size=4 * KiB)
reg(Download, "W1.dn.1k", "W1", count=1000, size=4 * KiB)
# Rows on the shared RAID (`.disk`) are kept small and few (io.max caps the containers at
# 200 MB/s write anyway): <= 1 GiB per run, at most 3–5 rounds.
DISK_NOTE = "disk rows scaled down for the shared host (<= 1 GiB written per run, io.max 200 MB/s)"
reg(Upload, "W1.up.10k.disk", "W1", count=10_000, size=4 * KiB, fsroot=DISK, max_rounds=5)
reg(Download, "W1.dn.10k.disk", "W1", count=10_000, size=4 * KiB, fsroot=DISK, max_rounds=5)
for w in (16, 64, 256):
    reg(Upload, f"W1.up.100k.w{w}", "W1", long=True, count=100_000, size=4 * KiB,
        gflags=("--numworkers", str(w)))
    reg(Download, f"W1.dn.100k.w{w}", "W1", long=True, count=100_000, size=4 * KiB,
        gflags=("--numworkers", str(w)))
# MinIO keeps its data on a 9 GiB tmpfs (no disk I/O); its large rows are scaled to <= 2 GiB so
# a round plus the previous round's not-yet-purged trash fits.
MINIO_NOTE = "scaled down on MinIO (data on a 9 GiB tmpfs)"
W2_MINIO = {"minio": {"count": 2000, "scaled": MINIO_NOTE + ": 2000 x 1 MiB"}}
reg(Upload, "W2.up", "W2", long=True, count=5000, size=MiB, on=W2_MINIO)
reg(Download, "W2.dn", "W2", long=True, count=5000, size=MiB, on=W2_MINIO)
reg(Upload, "W2.up.disk", "W2", long=True, count=500, size=MiB, fsroot=DISK, max_rounds=3,
    scaled=DISK_NOTE + ": 500 x 1 MiB", endpoints=STUB_ONLY)
reg(Download, "W2.dn.disk", "W2", long=True, count=500, size=MiB, fsroot=DISK, max_rounds=3,
    scaled=DISK_NOTE + ": 500 x 1 MiB", endpoints=STUB_ONLY)
W3_MINIO = {"1x4g": {"minio": {"size": GiB, "scaled": MINIO_NOTE + ": 1 x 1 GiB"}},
            "4x1g": {"minio": {"size": 512 * MiB, "scaled": MINIO_NOTE + ": 4 x 512 MiB"}},
            "32x128m": {"minio": {"count": 16, "scaled": MINIO_NOTE + ": 16 x 128 MiB"}}}
for count, size, tag in ((1, 4 * GiB, "1x4g"), (4, GiB, "4x1g"), (32, 128 * MiB, "32x128m")):
    on = W3_MINIO[tag]
    reg(Upload, f"W3.up.{tag}", "W3", long=True, count=count, size=size, on=on)
    reg(Download, f"W3.dn.{tag}", "W3", long=True, count=count, size=size, on=on)
    reg(Upload, f"W3.up.{tag}.eq", "W3", long=True, count=count, size=size, flags=S5_EQ, on=on)
    reg(Download, f"W3.dn.{tag}.eq", "W3", long=True, count=count, size=size, flags=S5_EQ, on=on)
# #14 at the stub's scale: many parts in flight (the issue ran -c 200 -p 32 on 100 GB), the same
# flags for every client.
C64 = {c: ("--concurrency", "64", "--part-size", "32") for c in ("rs5cmd", "rs5cmd-fast")}
C64["s5cmd"] = ("-c", "64", "-p", "32")
for count, size, tag in ((1, 4 * GiB, "1x4g"), (4, GiB, "4x1g")):
    reg(Download, f"W3.dn.{tag}.c64", "W3", long=True, count=count, size=size, flags=C64,
        endpoints=("stub-tls", "stub-h2"))
reg(Upload, "W3.up.4x256m.disk", "W3", long=True, count=4, size=256 * MiB, fsroot=DISK,
    max_rounds=3, scaled=DISK_NOTE + ": 4 x 256 MiB", endpoints=STUB_ONLY)
reg(Download, "W3.dn.4x256m.disk", "W3", long=True, count=4, size=256 * MiB, fsroot=DISK,
    max_rounds=3, scaled=DISK_NOTE + ": 4 x 256 MiB", endpoints=STUB_ONLY)
# Scaled down on MinIO (caps: memory, time): 1M-key listings and 100k-key rm run on the stub
# only; MinIO gets the 100k listing and du, and rm of 10k keys (re-seeded every round).
reg(Ls, "W4.ls.100k", "W4", count=100_000)
reg(Ls, "W4.ls.1m", "W4", long=True, count=1_000_000, endpoints=STUB_ONLY)
reg(Ls, "W10.du.100k", "W10", count=100_000, du=True)
reg(Ls, "W10.du.1m", "W10", long=True, count=1_000_000, du=True, endpoints=STUB_ONLY)
for d in ("up", "dn"):
    for mode in ("cold", "warm", "changed", "delete"):
        reg(SyncStub, f"W5.{d}.{mode}", "W5", long=False, count=100_000, dir=d, mode=mode)
    reg(SyncMinio, f"W5.minio.{d}", "W5", count=10_000, dir=d)
reg(Rm, "W6.rm.100k", "W6", long=True, count=100_000, endpoints=STUB_ONLY)
reg(Rm, "W6.rm.10k", "W6", count=10_000)
W7_MINIO = {"minio": {"size": GiB, "scaled": MINIO_NOTE + ": 1 GiB object"}}
reg(Cat, "W7.cat", "W7", long=True, size=4 * GiB, on=W7_MINIO)
reg(Pipe, "W7.pipe", "W7", long=True, size=4 * GiB, on=W7_MINIO)
reg(RunFile, "W8.run.up", "W8", count=10_000, dir="up")
reg(RunFile, "W8.run.dn", "W8", count=10_000, dir="dn")
for op in ("version", "ls1", "cp1"):
    reg(Tiny, f"W9.{op}", "W9", op=op)
    reg(Tiny, f"W9.{op}.noregion", "W9", op=op, region=False)


# ------------------------------------------------------------------------------------------------
# Measurement
# ------------------------------------------------------------------------------------------------
def loadavg():
    with open("/proc/loadavg") as f:
        return float(f.read().split()[0])


def iouring_allowed():
    return subprocess.run([PROBE], capture_output=True).returncode == 0


def wrap_argv(ctx, row, client, rnd):
    """--wrap 'perf record -o /x/{row}-{client}-{round}.data --' -> argv prefix."""
    if not ctx.args.wrap:
        return []
    import shlex
    return [w.format(row=row.id, client=client, round=rnd, endpoint=ctx.endpoint)
            for w in shlex.split(ctx.args.wrap)]


MINIO_DATA = os.environ.get("MINIO_DATA", "/minio-data")
# Unmeasured seeding uploads (MinIO) stay gentle: with 64 workers they pushed host load past 12.
SEED_WORKERS = os.environ.get("SEED_WORKERS", "16")


def purge_minio_trash(ctx):
    """Empty MinIO's .minio.sys/tmp/.trash (deleted objects waiting for MinIO's own, too slow,
    cleaner). Its data is a 9 GiB tmpfs; without this, churny rows fill it within minutes."""
    if ctx.endpoint != "minio":
        return
    t = os.path.join(MINIO_DATA, ".minio.sys", "tmp", ".trash")
    try:
        entries = list(os.scandir(t))
    except OSError:
        return
    for e in entries:
        if e.is_dir(follow_symlinks=False):
            shutil.rmtree(e.path, ignore_errors=True)
        else:
            try:
                os.unlink(e.path)
            except OSError:
                pass


LOAD_MAX = float(os.environ.get("LOAD_MAX", "11"))  # shared host: keep load1 under ~12
LOAD_WAIT_MAX = float(os.environ.get("LOAD_WAIT_MAX", "inf"))  # never start a run above LOAD_MAX


def wait_for_load():
    """Wait (up to LOAD_WAIT_MAX s) while the host's 1-minute load average is above LOAD_MAX.
    Returns the seconds waited."""
    t0 = time.time()
    noted = False
    while loadavg() > LOAD_MAX and time.time() - t0 < LOAD_WAIT_MAX:
        if not noted:
            print(f"   host load {loadavg():.1f} > {LOAD_MAX:g}: waiting", flush=True)
            noted = True
        time.sleep(10)
    return round(time.time() - t0, 1)


def time_wait_sockets():
    n = 0
    for p in ("/proc/net/tcp", "/proc/net/tcp6"):
        try:
            with open(p) as f:
                next(f)
                n += sum(1 for line in f if line.split()[3] == "06")
        except OSError:
            pass
    return n


def wait_for_ports(ctx, limit=2000, max_wait=90):
    """MinIO is not on loopback, so TIME_WAIT ports are not reused (tcp_tw_reuse=2): a client
    that opens a connection per command (rs5cmd `run`) can exhaust the ~28k ephemeral ports if
    the previous round's sockets are still in TIME_WAIT. Start each MinIO run with < `limit`."""
    if ctx.endpoint != "minio":
        return 0.0
    try:
        with open("/proc/sys/net/ipv4/tcp_tw_reuse") as f:
            if f.read().strip() == "1":
                return 0.0  # TIME_WAIT ports are reusable (compose sets this for bench)
    except OSError:
        pass
    t0 = time.time()
    while time_wait_sockets() > limit and time.time() - t0 < max_wait:
        time.sleep(2)
    return round(time.time() - t0, 1)


def measure(ctx, row, client, rnd, warm, run, out_mode, results, extra=None):
    tag = f"{client}-r{rnd}"
    load_wait = wait_for_load()
    tw_before = time_wait_sockets()
    port_wait = wait_for_ports(ctx)
    out_path = None
    if warm or out_mode == "file":
        out_path = f"{TMPFS}/stdout-{tag}.txt"
    jpath = f"{TMPFS}/launch-{os.getpid()}.json"
    if os.path.exists(jpath):
        os.unlink(jpath)
    err_path = f"{TMPFS}/stderr-{tag}.txt"
    timeout = ctx.args.timeout
    argv = [LAUNCH, "-c", CPU_CLIENT, "-g", f"client={CPU_CLIENT}", "-g", f"server={CPU_SERVER}",
            "-g", f"other={CPU_OTHER}", "-o", jpath, "-t", str(timeout), "--"] + \
        wrap_argv(ctx, row, client, rnd) + run.argv
    strace_path = None
    if warm and CLIENTS[client].get("fast"):
        # Which driver did --fast use? Count io_uring vs epoll calls on the warm-up run only.
        strace_path = f"{TMPFS}/strace-{tag}.txt"
        argv = argv[: argv.index("--") + 1] + ["strace", "-f", "-c", "-o", strace_path,
                                               "-e", "trace=io_uring_setup,io_uring_enter,epoll_wait,epoll_pwait"] + run.argv
    uring = iouring_allowed() if CLIENTS[client].get("fast") else None
    stdin = open(run.stdin, "rb") if run.stdin else subprocess.DEVNULL
    if out_path:
        stdout = open(out_path, "wb")
    elif out_mode == "pipe":
        stdout = subprocess.PIPE
    else:
        stdout = subprocess.DEVNULL
    if ctx.stub:
        ctx.stub.reset()
        s0 = ctx.stub.cpu_s()
    env = dict(run.env or ctx.env())
    env.update(CLIENTS[client].get("env", {}))
    load0 = loadavg()
    t0 = time.time()
    if out_mode == "pipe" and not out_path:
        p = subprocess.Popen(argv, env=env, stdin=stdin, stdout=subprocess.PIPE,
                             stderr=open(err_path, "wb"))
        sink = subprocess.Popen(["taskset", "-c", CPU_CLIENT, "cat"], stdin=p.stdout,
                                stdout=subprocess.DEVNULL)
        p.stdout.close()
        rc = p.wait()
        sink.wait()
    else:
        rc = subprocess.run(argv, env=env, stdin=stdin, stdout=stdout,
                            stderr=open(err_path, "wb")).returncode
    if run.stdin:
        stdin.close()
    stats = None
    rec = {}
    if ctx.stub:
        stub_cpu = ctx.stub.cpu_s() - s0
        stats = ctx.stub.stats()
        rec["stub_cpu_s"] = round(stub_cpu, 2)
        rec["stub"] = {k: v for k, v in stats.items()
                       if k in ("conns", "conns_peak", "conns_h2", "reqs", "faults", "tls_failed")
                       or (k.startswith("kx_") and v)}
    with open(jpath) as f:
        rec.update(json.loads(f.read().strip().splitlines()[-1]))
    ok, info = (False, {"error": "nonzero exit"}) if rc != 0 else row.check(
        ctx, client, tag, run, stats, out_path)
    if not ok and out_path and os.path.exists(out_path):
        # Keep what a failed run printed (its first 64 KiB): the check only says it was wrong.
        keep = f"{RESULTS}/fail-{ctx.args.tag}-{row.id}-{client}-r{rnd}.out"
        with open(out_path, "rb") as src, open(keep, "wb") as dst:
            dst.write(src.read(64 * 1024))
        info["kept_stdout"] = os.path.basename(keep)
    if rc != 0:
        with open(err_path, "rb") as f:
            info["stderr"] = f.read()[-600:].decode(errors="replace")
    if strace_path and os.path.exists(strace_path):
        with open(strace_path) as f:
            calls = {}
            for line in f:
                parts = line.split()
                if len(parts) >= 5 and parts[-1] in ("io_uring_setup", "io_uring_enter",
                                                     "epoll_wait", "epoll_pwait"):
                    calls[parts[-1]] = int(parts[3]) if parts[3].isdigit() else parts[3]
        info["syscalls"] = calls
    label = client
    if CLIENTS[client].get("fast"):
        label = f"{client}-{'uring' if uring else 'epoll'}"  # rs5cmd-fast -> rs5cmd-fast-uring
    rec.update({
        "ts": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(t0)), "tag": ctx.args.tag,
        "row": row.id, "workload": ROWS[row.id][1], "endpoint": ctx.endpoint, "client": label,
        "round": rnd, "warmup": warm, "stdout": "file" if out_path else out_mode,
        "argv": [a for a in run.argv], "ok": bool(ok), "check": info, "load1": load0,
        "iouring_allowed": uring, "stub_cpus": ctx.args.stub_cpus if ctx.stub else None,
        "load_wait_s": load_wait, "time_wait_before": tw_before, "port_wait_s": port_wait,
        "time_wait_after": time_wait_sockets(),
    })
    if getattr(row, "p", {}).get("scaled"):
        rec["scaled"] = row.p["scaled"]
    if extra:
        rec.update(extra)
    results.write(json.dumps(rec) + "\n")
    results.flush()
    for p in (out_path, err_path):
        if p and os.path.exists(p) and not ctx.args.keep_output:
            os.unlink(p)
    status = "ok" if ok else "FAIL"
    print(f"  {row.id:<20} {label:<18} r{rnd}{'w' if warm else ' '} wall {rec['wall_s']:8.3f}s "
          f"cpu {rec['user_s'] + rec['sys_s']:7.2f}s rss {rec['maxrss_kb'] / 1024:7.1f}MB "
          f"{status}" + ("" if ok else f" {json.dumps(info)[:300]}"), flush=True)
    return rec


def run_rows(args):
    os.makedirs(RESULTS, exist_ok=True)
    os.makedirs(TMPFS, exist_ok=True)
    ctx = Ctx(args)
    ids = [r for r in ROWS if any(fnmatch.fnmatch(r, pat) for pat in args.rows.split(","))]
    unmatched = [pat for pat in args.rows.split(",") if not any(fnmatch.fnmatch(r, pat) for r in ROWS)]
    if unmatched:  # a misspelt row would otherwise be skipped without a word
        sys.exit(f"no rows match {','.join(unmatched)}")
    if not ids:
        sys.exit(f"no rows match {args.rows}")
    clients = args.clients.split(",")
    for c in clients:
        if c not in CLIENTS:
            sys.exit(f"unknown client {c}")
    if args.endpoint.startswith("stub"):
        env_extra = {}
        if args.latency_ms:
            env_extra["STUB_LATENCY_MS"] = str(args.latency_ms)
        if args.fault_rate:
            env_extra["STUB_FAULT_RATE"] = str(args.fault_rate)
        if args.stub_close:
            env_extra["STUB_CONN_CLOSE"] = "1"
        ctx.stub = Stub(args.stub_cpus, env_extra).start()
    results = open(os.path.join(RESULTS, f"{args.tag}.jsonl"), "a")
    extra = {}
    if args.latency_ms:
        extra["latency_ms"] = args.latency_ms
    if args.fault_rate:
        extra["fault_rate"] = args.fault_rate
    if args.variant:
        extra["variant"] = args.variant
    if args.stub_close:
        extra["conn_close"] = True
    if args.env:
        extra["env"] = args.env
    if args.wrap:
        extra["wrap"] = args.wrap
    try:
        for rid in ids:
            cls, workload, long, p = ROWS[rid]
            p = dict(p)
            ov = p.pop("on", {}).get(ctx.endpoint)
            if ov:  # endpoint-specific sizes (rows scaled down to stay inside the caps)
                p.update(ov)
            row = cls(rid, **p)
            if ctx.endpoint not in row.endpoints:
                print(f"skip {rid}: not for endpoint {ctx.endpoint}")
                continue
            row_clients = [c for c in clients if not CLIENTS[c].get("fast") or row.fast_ok]
            if not row_clients:
                continue
            rounds = args.rounds or (5 if (long if long is not None else row.long) else 10)
            if p.get("max_rounds"):
                rounds = min(rounds, p["max_rounds"])
            print(f"== {rid} on {ctx.endpoint}: {', '.join(row_clients)}; {rounds} rounds + "
                  f"{args.warmup} warm-up", flush=True)
            t = time.time()
            purge_minio_trash(ctx)
            row.prepare(ctx)
            print(f"   prepared in {time.time() - t:.1f}s", flush=True)
            for rnd in range(args.warmup + rounds):
                warm = rnd < args.warmup
                k = rnd % len(row_clients)
                order = row_clients[k:] + row_clients[:k]
                for c in order:
                    tag = f"{c}-r{rnd}"
                    if isinstance(row, SyncMinio):
                        for mode in row.chain():
                            run = row.setup_step(ctx, c, tag, mode)
                            step = type("Step", (), {})()
                            step.id = f"{rid}.{mode}"
                            step.check = (lambda ctx_, client_, tag_, run_, stats_, out_, m=mode:
                                          row.check_step(ctx_, client_, tag_, m, out_))
                            ROWS.setdefault(step.id, (SyncMinio, workload, False, p))
                            measure(ctx, step, c, rnd, warm, run, args.stdout, results, extra)
                        row.end_chain(ctx, c, tag)
                        purge_minio_trash(ctx)
                        continue
                    run = row.setup(ctx, c, tag)
                    measure(ctx, row, c, rnd, warm, run, args.stdout, results, extra)
                    row.teardown(ctx, c, tag)
                    purge_minio_trash(ctx)
                    if args.pause:
                        time.sleep(args.pause)
            row.finish(ctx)
            purge_minio_trash(ctx)
    finally:
        if ctx.stub:
            ctx.stub.stop()
        results.close()
        uid, gid = int(os.environ.get("HOST_UID", "0")), int(os.environ.get("HOST_GID", "0"))
        if uid:
            for root, dirs, files in os.walk(RESULTS):
                for n in dirs + files:
                    os.chown(os.path.join(root, n), uid, gid)
            os.chown(RESULTS, uid, gid)


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("list")
    r = sub.add_parser("run")
    r.add_argument("--rows", required=True, help="comma-separated globs over row ids")
    r.add_argument("--endpoint", default="stub", choices=sorted(ENDPOINTS))
    r.add_argument("--clients", default="s5cmd,rs5cmd")
    r.add_argument("--rounds", type=int, default=0, help="0 = 5 for long rows, 10 for short")
    r.add_argument("--warmup", type=int, default=1)
    r.add_argument("--tag", default="adhoc")
    r.add_argument("--variant", default="")
    r.add_argument("--stub-cpus", default=CPU_SERVER)
    r.add_argument("--stdout", default="null", choices=("null", "pipe", "file"))
    r.add_argument("--latency-ms", type=float, default=0)
    r.add_argument("--fault-rate", type=float, default=0)
    r.add_argument("--timeout", type=float, default=900)
    r.add_argument("--pause", type=float, default=0.5)
    r.add_argument("--keep-output", action="store_true")
    r.add_argument("--client-bin", action="append", default=[],
                   help="NAME=PATH: an extra client (rs5cmd* names behave as rs5cmd; "
                        "a name ending in -fast adds --fast)")
    r.add_argument("--env", action="append", default=[], help="KEY=VAL for every client run")
    r.add_argument("--wrap", default="", help="program prefixed to every measured command")
    r.add_argument("--stub-close", action="store_true",
                   help="the stub ends every connection after one response (one handshake per request)")
    r.add_argument("--client-flag", action="append", default=[],
                   help="NAME=FLAG: a global flag for client NAME only (repeatable)")
    r.add_argument("--client-env", action="append", default=[],
                   help="NAME=KEY=VALUE: an environment variable for client NAME only (repeatable)")
    args = ap.parse_args()
    if args.cmd == "list":
        for rid, (cls, wl, long, p) in ROWS.items():
            print(f"{rid:<22} {wl:<4} {cls.__name__:<10} {'long' if long else 'short'} "
                  f"{','.join(p.get('endpoints', cls.endpoints))}")
        return
    for spec in args.client_bin:
        name, path = spec.split("=", 1)
        CLIENTS[name] = {"bin": path, "fast": name.endswith("-fast")}
    for spec in args.client_flag:
        name, flag = spec.split("=", 1)
        CLIENTS[name].setdefault("global_flags", []).append(flag)
    for spec in args.client_env:
        name, kv = spec.split("=", 1)
        k, v = kv.split("=", 1)
        CLIENTS[name].setdefault("env", {})[k] = v
    run_rows(args)


if __name__ == "__main__":
    main()
