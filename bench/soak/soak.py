#!/usr/bin/env python3
"""Soak runner. Runs inside the `soak` container, started by bench/soak/soak.sh on the host.

    soak.py prepare
    soak.py run --target dry|aws|rgw --scale dry|full [--only S1,S4] [--skip S13] [--keep]
                [--client-bin NAME=PATH]... [--client-flag NAME=FLAG]... [--client-env NAME=K=V]...
    soak.py cleanup --target T --run-id ID
    soak.py report --run-id ID

Clients are configuration (name -> binary, global flags, environment), as in
bench/review/matrix.py: a name that starts with `s5cmd` speaks s5cmd's dialect, any other name
rs5cmd's. The rs5cmd binary is SOAK_RS5CMD_BIN (a name in the review's bins volume, default
`b-stage`). If it has the data-path switch, it runs as three clients that differ only by flags:
rs5cmd-sdk, rs5cmd-lean and rs5cmd-lean-tr (lean with --checksum-trailers, uploads of S1, S11 and
S14 only); otherwise as one client, rs5cmd. s5cmd is the v2.3.0 release.

Every measured command runs under `launch` on the soak's two CPUs. Before and during each command
the runner reads the host guard's heartbeat (conntrack count, rate limit in place), holds while
the host's load1 is above LOAD_MAX, samples the container's interface counters against the rate
cap and keeps each scenario inside its share of the transfer volume. Credentials are read from
the mounted env file, handed to the clients only through their environment, and replaced in
everything that is written to results/.
"""
import argparse
import hashlib
import itertools
import json
import os
import re
import shutil
import signal
import statistics
import subprocess
import sys
import time
import traceback

sys.dont_write_bytecode = True
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
if __name__ == "__main__":   # scenarios.py imports this module by name: keep one copy of its classes
    sys.modules.setdefault("soak", sys.modules[__name__])
import s3check  # noqa: E402
from s3check import S3Error  # noqa: E402

RESULTS = os.path.join(HERE, "results")
HOSTDIR = "/run/soak-host"
CREDS = "/run/soak-creds"
SCRATCH = "/mnt/scratch"
PRIV = "/run/soak"
BINS = "/opt/bins"
REVIEW_BINS = "/opt/review-bins"
LAUNCH = f"{BINS}/launch"
RS5CMD_BIN = os.environ.get("SOAK_RS5CMD_BIN") or "b-stage"
CPUS = os.environ.get("SOAK_CPUS", "3,11")
CAP_BYTES = 40_000_000          # the hard rate limit, bytes/s each way
LOAD_MAX = float(os.environ.get("LOAD_MAX", "11"))
CT_WARN, CT_ABORT = 10_000, 30_000
SCRATCH_MAX = 24 * 1000**3      # stop before the 25 GB of scratch are used
MiB = 1 << 20
RETRY_ENV = "aws_smithy_runtime::client::retries::strategy::standard=debug"
RETRY_RE = {"s5cmd": re.compile(r"retryable error"), "rs5cmd": re.compile(r"attempt #\d+ failed with")}
THROTTLE_RE = re.compile(r"SlowDown|ThrottlingError|\b503\b|Slow Down|TooManyRequests|RequestLimitExceeded")
ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
DEBUG_RE = re.compile(r"^DEBUG | DEBUG ")


class Abort(Exception):
    """Stop the whole soak now (guard, rate cap, heartbeat)."""


class OverBudget(Exception):
    """A scenario went beyond its share of the transfer volume: stop and report."""


class Skip(Exception):
    """This scenario does not apply to this target."""


# --- targets ---------------------------------------------------------------------------------

def parse_env_file(path):
    out = {}
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            if line.startswith("export "):
                line = line[7:].lstrip()
            if "=" not in line:
                continue
            k, v = line.split("=", 1)
            v = v.strip()
            if len(v) >= 2 and v[0] == v[-1] and v[0] in "'\"":
                v = v[1:-1]
            out[k.strip()] = v
    return out


class Target:
    """An endpoint and its credentials, from an env file. Values never leave this object except
    into a client's environment and the checker."""

    def __init__(self, name):
        path = os.path.join(HERE, "targets", "dry.env") if name == "dry" else f"{CREDS}/{name}.env"
        if not os.path.exists(path):
            raise SystemExit(f"soak: the credentials file for target '{name}' is not there")
        e = parse_env_file(path)
        need = ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "SOAK_BUCKET"]
        if name != "aws":
            need.append("SOAK_ENDPOINT_URL")
        missing = [k for k in need if not e.get(k)]
        if missing:
            raise SystemExit(f"soak: target '{name}': missing {', '.join(missing)}")
        self.name = name
        self.kind = e.get("SOAK_KIND") or {"dry": "minio", "aws": "aws", "rgw": "rgw"}.get(name, name)
        self.ak, self.sk, self.token = e["AWS_ACCESS_KEY_ID"], e["AWS_SECRET_ACCESS_KEY"], e.get("AWS_SESSION_TOKEN")
        self.region = e.get("AWS_REGION") or "us-east-1"
        self.bucket = e["SOAK_BUCKET"]
        self.role_arn = e.get("AWS_ROLE_ARN")
        self.endpoint = e.get("SOAK_ENDPOINT_URL")
        self.addressing = e.get("SOAK_ADDRESSING")
        ca = e.get("SOAK_CA_FILE")
        self.ca_file = os.path.join(CREDS, os.path.basename(ca)) if ca else None
        self.version = e.get("SOAK_RGW_VERSION")
        self.denied_bucket = e.get("SOAK_DENIED_BUCKET")        # exists, outside the policy (S8)
        self.public_url = e.get("SOAK_PUBLIC_URL")              # s3://bucket/key, anonymous (S12)
        self.public_region = e.get("SOAK_PUBLIC_REGION")
        if self.kind == "aws" and not self.public_url:           # an AWS Open Data bucket
            self.public_url, self.public_region = "s3://noaa-ghcn-pds/ghcnd-countries.txt", "us-east-1"
        self.throttle_endpoint = e.get("SOAK_THROTTLE_ENDPOINT_URL")  # dry run: the 503 injector
        default_sc = {"aws": "STANDARD_IA,GLACIER_IR", "minio": "REDUCED_REDUNDANCY"}.get(self.kind, "")
        self.storage_classes = [s for s in e.get("SOAK_STORAGE_CLASSES", default_sc).split(",") if s]

    def checker(self):
        return s3check.S3(self.ak, self.sk, self.region, endpoint_url=self.endpoint,
                          addressing=self.addressing, session_token=self.token, ca_file=self.ca_file)

    def secrets(self):
        pairs = [(self.sk, "<secret>"), (self.token, "<token>"), (self.ak, "<access-key-id>"),
                 (self.role_arn, "<role-arn>"), (self.bucket, "<bucket>")]
        if self.role_arn:
            m = re.search(r"\d{12}", self.role_arn)
            if m:
                pairs.append((m.group(0), "<account>"))
        for url in (self.endpoint, self.throttle_endpoint):
            if url and self.name != "dry":
                host = url.split("://", 1)[-1].split("/", 1)[0]
                pairs += [(host, "<endpoint>"), (host.split(":")[0], "<endpoint>")]
        if self.denied_bucket:
            pairs.append((self.denied_bucket, "<denied-bucket>"))
        return [(v, p) for v, p in pairs if v]


class Redactor:
    def __init__(self, pairs):
        self.pairs = sorted(pairs, key=lambda vp: -len(vp[0]))

    def __call__(self, s):
        for v, p in self.pairs:
            s = s.replace(v, p)
        return s


# --- clients ---------------------------------------------------------------------------------

class Client:
    def __init__(self, name, path):
        self.name, self.bin, self.flags, self.env = name, path, [], {}
        self.kind = "s5cmd" if name.startswith("s5cmd") else "rs5cmd"
        self.only = None            # the scenarios this client takes part in (None: all of them)
        self.uploads_only = False   # there, only the upload steps (the trailers client)
        self._help = {}

    @property
    def path(self):
        """rs5cmd's data path: `lean` or `sdk` (also the default); None for other clients."""
        if self.kind != "rs5cmd":
            return None
        val = self.env.get("RS5CMD_DATA_PATH", "sdk")
        for i, f in enumerate(self.flags):
            if f.startswith("--data-path="):
                val = f.split("=", 1)[1]
            elif f == "--data-path" and i + 1 < len(self.flags):
                val = self.flags[i + 1]
        return val

    @property
    def trailers(self):
        return self.path == "lean" and "--checksum-trailers" in self.flags

    def twin(self, other):
        """The same rs5cmd binary under other flags: the two must behave alike."""
        return self is not other and self.kind == other.kind == "rs5cmd" and self.bin == other.bin

    def help(self, sub=None):
        if sub not in self._help:
            argv = [self.bin] + ([sub] if sub else []) + ["--help"]
            p = subprocess.run(argv, capture_output=True, text=True, timeout=30,
                               env={"PATH": "/usr/bin:/bin", "HOME": "/nonexistent", "NO_COLOR": "1"})
            self._help[sub] = p.stdout + p.stderr
        return self._help[sub]

    def has(self, sub, flag):
        return re.search(r"(^|[\s,])" + re.escape(flag) + r"([\s,=]|$)", self.help(sub), re.M) is not None

    def version(self):
        argv = [self.bin, "version"] if self.kind == "s5cmd" else [self.bin, "--version"]
        p = subprocess.run(argv, capture_output=True, text=True, timeout=30, env={"PATH": "/usr/bin:/bin"})
        return (p.stdout + p.stderr).strip()

    def sha256(self):
        return file_hashes(self.bin)["sha256"]


def default_clients():
    """One rs5cmd binary and s5cmd. With the data-path switch the binary runs as its SDK path, its
    lean path and its lean path with trailers; rs5cmd-sdk comes first, so it is what the others
    are compared with."""
    path, s5 = f"{BINS}/{RS5CMD_BIN}", Client("s5cmd", f"{BINS}/s5cmd")
    one = Client("rs5cmd", path)
    if not os.path.exists(path) or not one.has(None, "--data-path"):
        return {"rs5cmd": one, "s5cmd": s5}
    sdk, lean, tr = Client("rs5cmd-sdk", path), Client("rs5cmd-lean", path), Client("rs5cmd-lean-tr", path)
    sdk.flags, lean.flags, tr.flags = ["--data-path=sdk"], ["--data-path=lean"], ["--data-path=lean", "--checksum-trailers"]
    tr.only, tr.uploads_only = {"S1", "S11", "S14"}, True
    return {c.name: c for c in (sdk, lean, tr, s5)}


# --- files -----------------------------------------------------------------------------------

def file_hashes(path):
    sha, md5, n = hashlib.sha256(), hashlib.md5(), 0
    with open(path, "rb") as f:
        while True:
            b = f.read(4 * MiB)
            if not b:
                break
            sha.update(b)
            md5.update(b)
            n += len(b)
    return {"size": n, "sha256": sha.hexdigest(), "md5": md5.hexdigest()}


def gen_file(path, size, seed):
    """A deterministic, incompressible file (AES-CTR keystream of a key made from the seed)."""
    key = hashlib.sha256(seed.encode()).hexdigest()[:32]
    sha, md5 = hashlib.sha256(), hashlib.md5()
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        if size:
            with open("/dev/zero", "rb") as z:
                p = subprocess.Popen(["openssl", "enc", "-aes-128-ctr", "-K", key, "-iv", "0" * 32],
                                     stdin=z, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
            left = size
            try:
                while left:
                    b = p.stdout.read(min(left, 4 * MiB))
                    if not b:
                        raise RuntimeError("openssl stream ended early")
                    f.write(b)
                    sha.update(b)
                    md5.update(b)
                    left -= len(b)
            finally:
                p.kill()
                p.wait()
    return {"size": size, "sha256": sha.hexdigest(), "md5": md5.hexdigest()}


def small_bytes(seed, size):
    return hashlib.shake_128(seed.encode()).digest(size)


def multipart_etag(path, part_size):
    digests = []
    with open(path, "rb") as f:
        while True:
            m, left = hashlib.md5(), part_size
            while left:
                b = f.read(min(left, 4 * MiB))
                if not b:
                    break
                m.update(b)
                left -= len(b)
            if left == part_size:
                break
            digests.append(m.digest())
    return f"{hashlib.md5(b''.join(digests)).hexdigest()}-{len(digests)}"


def link_bytes():
    out = []
    for n in ("tx", "rx"):
        with open(f"/sys/class/net/eth0/statistics/{n}_bytes") as f:
            out.append(int(f.read()))
    return tuple(out)


def du(path):
    p = subprocess.run(["du", "-sb", path], capture_output=True, text=True)
    return int(p.stdout.split()[0]) if p.returncode == 0 and p.stdout else 0


def cap_text(s, head=48 * 1024, tail=16 * 1024):
    if len(s) <= head + tail:
        return s
    return s[:head] + f"\n... [{len(s) - head - tail} characters cut] ...\n" + s[-tail:]


class Res:
    """One finished command."""

    def __init__(self, **kw):
        self.__dict__.update(kw)

    @property
    def lines(self):
        return [ln for ln in self.out.splitlines() if ln.strip()]


# --- the run ---------------------------------------------------------------------------------

class Run:
    def __init__(self, target, clients, scale, run_id, scale_name):
        self.t, self.clients, self.scale, self.id, self.scale_name = target, clients, scale, run_id, scale_name
        self.all_clients = clients
        self.excluded = {}          # client name -> why it sits out the rest of the run
        self.endpoint = {}          # what the probe learned about the endpoint
        self.dir = os.path.join(RESULTS, run_id)
        self.scratch = os.path.join(SCRATCH, run_id)
        self.live = os.path.join(self.scratch, "live")
        os.makedirs(self.dir, exist_ok=True)
        os.makedirs(self.live, exist_ok=True)
        os.makedirs(PRIV, exist_ok=True)
        self.s3 = target.checker()
        self.redact = Redactor(target.secrets() + [(self.scratch, "<scratch>")])
        self.files = {n: open(os.path.join(self.dir, f"{n}.jsonl"), "a", encoding="utf-8")
                      for n in ("steps", "outputs", "events", "findings")}
        self.base = f"soak/{run_id}"
        self.scn = "setup"
        self.stats = {}
        self.manifest = {}
        self.etag_cache = {}
        self.state = {"extra_keys": [], "scratch_buckets": []}
        self.rate = CAP_BYTES
        self.seq = itertools.count(1)
        self.g = {}
        self.max_conntrack = 0
        self._ct_warned = 0
        self._du_tick = 0
        self.t_start = time.time()
        self.load_state()

    # --- bookkeeping ---
    def say(self, msg):
        print(self.redact(msg), flush=True)

    def write(self, name, rec):
        rec = {"ts": round(time.time(), 3), "scn": self.scn, **rec}
        self.files[name].write(self.redact(json.dumps(rec, ensure_ascii=False)) + "\n")
        self.files[name].flush()

    def event(self, kind, **kw):
        self.write("events", {"event": kind, **kw})

    def finding(self, kind, title, detail=None, client=None, argv=None):
        """kind: bug (data or behaviour wrong), path (the same binary differs between its data
        paths), diff (clients differ), gap (flag missing), endpoint (the endpoint behaves
        unexpectedly), note."""
        self.write("findings", {"kind": kind, "title": title, "client": client, "detail": detail, "argv": argv})
        self.say(f"   finding [{kind}] {title}")

    def load_state(self):
        p = os.path.join(self.dir, "state.json")
        if os.path.exists(p):
            with open(p) as f:
                self.state.update(json.load(f))

    def save_state(self):
        # Bucket names and keys only; written through the redactor like everything else.
        with open(os.path.join(self.dir, "state.json"), "w") as f:
            f.write(self.redact(json.dumps(self.state)))

    def note_bucket(self, suffix):
        if suffix not in self.state["scratch_buckets"]:
            self.state["scratch_buckets"].append(suffix)
            self.save_state()

    def note_key(self, key):
        if key not in self.state["extra_keys"]:
            self.state["extra_keys"].append(key)
            self.save_state()

    # --- names ---
    def prefix(self, c, *parts):
        return "/".join([self.base, c.name, self.scn.lower(), *parts])

    def url(self, c, *parts):
        return f"s3://{self.t.bucket}/" + self.prefix(c, *parts)

    def work(self, c, *parts):
        p = os.path.join(self.scratch, c.name, self.scn.lower(), *parts)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        return p

    def src(self, *parts):
        p = os.path.join(self.scratch, "src", *parts)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        return p

    # --- guards ---
    def guard(self):
        """Raise Abort when the host guard says so, the conntrack table is too full, the rate
        limit is not in place or the heartbeat is lost."""
        t0 = time.time()
        while True:
            if os.path.exists(f"{HOSTDIR}/abort"):
                with open(f"{HOSTDIR}/abort") as f:
                    raise Abort("host guard: " + f.read().strip())
            try:
                with open(f"{HOSTDIR}/guard.json") as f:
                    g = json.load(f)
            except (OSError, ValueError):
                g = None
            if g and time.time() - g["ts"] <= 45:
                break
            if time.time() - t0 > 60:
                raise Abort("host guard heartbeat lost for 60 s")
            time.sleep(1)
        ct = g["conntrack"]
        self.max_conntrack = max(self.max_conntrack, ct)
        if ct > CT_ABORT:
            raise Abort(f"conntrack count {ct} above {CT_ABORT}")
        if ct > CT_WARN and time.time() - self._ct_warned > 60:
            self._ct_warned = time.time()
            self.event("conntrack_warn", count=ct)
            self.say(f"   WARNING: host conntrack count {ct} above {CT_WARN}")
        if not g.get("tc_ok"):
            raise Abort("the rate limit is not in place")
        self.g = g
        return g

    def hold(self):
        """Wait while the host's 1-minute load average is above LOAD_MAX (as matrix.py does)."""
        t0, said = time.time(), False
        while True:
            self.guard()
            with open("/proc/loadavg") as f:
                load1 = float(f.read().split()[0])
            if load1 <= LOAD_MAX:
                break
            if not said:
                self.say(f"   host load {load1:.1f} > {LOAD_MAX:g}: waiting")
                said = True
            time.sleep(5)
        w = time.time() - t0
        if w > 1:
            self.event("load_hold", wait_s=round(w, 1))
        return round(w, 1), load1

    def set_rate(self, mbps):
        """Ask the host guard for a lower rate (never above the cap) and wait until it is set."""
        want = int(mbps * 1_000_000)
        if want > CAP_BYTES:
            raise Abort("rate request above the cap")
        tmp = f"{HOSTDIR}/rate.request.tmp"
        with open(tmp, "w") as f:
            f.write(f"{mbps}\n")
        os.replace(tmp, f"{HOSTDIR}/rate.request")
        t0 = time.time()
        while time.time() - t0 < 120:
            if self.guard().get("tc_rate") == want:
                self.rate = want
                self.event("rate", bytes_per_s=want)
                return
            time.sleep(1)
        raise Abort("rate change not confirmed by the host guard")

    def rate_check(self, samples):
        """The container's own counters against the cap: any window of 5 s or more."""
        now, tx, rx = samples[-1]
        worst = [0.0, 0.0]
        for t, a, b in samples[:-1]:
            dt = now - t
            if dt >= 4:
                worst = [max(worst[0], (tx - a) / dt), max(worst[1], (rx - b) / dt)]
        # the bucket (512 kB) and coarse sampling allow a little above the rate
        if max(worst) > self.rate * 1.10 + 200_000:
            raise Abort(f"rate cap not effective: {max(worst) / 1e6:.1f} MB/s over a window, cap {self.rate / 1e6:.1f}")
        return worst

    def budget_check(self):
        st = self.stats.get(self.scn)
        if not st:
            return
        tx, rx = link_bytes()
        up, down = tx - st["tx0"], rx - st["rx0"]
        share = self.scale["shares"].get(self.scn)
        if share and (up > share[0] * MiB or down > share[1] * MiB):
            raise OverBudget(f"{self.scn} used {up / MiB:.0f} MiB up / {down / MiB:.0f} MiB down, "
                             f"its share is {share[0]} / {share[1]} MiB")
        self._du_tick += 1
        used = du(self.scratch) if self._du_tick % 20 == 0 else 0
        if used > SCRATCH_MAX:
            raise Abort(f"local scratch at {used / 1e9:.1f} GB")

    # --- running a client ---
    def client_env(self, c, extra=None):
        home = f"{PRIV}/home/{c.name}"
        os.makedirs(home, exist_ok=True)
        t = self.t
        env = {"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": home, "LANG": "C.UTF-8", "NO_COLOR": "1",
               "AWS_ACCESS_KEY_ID": t.ak, "AWS_SECRET_ACCESS_KEY": t.sk,
               "AWS_REGION": t.region, "AWS_DEFAULT_REGION": t.region,
               "AWS_EC2_METADATA_DISABLED": "true"}
        if t.token:
            env["AWS_SESSION_TOKEN"] = t.token
        if t.endpoint:
            env["S3_ENDPOINT_URL"] = env["AWS_ENDPOINT_URL"] = t.endpoint
        if t.ca_file:
            env["SSL_CERT_FILE"] = env["AWS_CA_BUNDLE"] = t.ca_file
        env.update(c.env)
        for k, v in (extra or {}).items():
            if v is None:
                env.pop(k, None)
            else:
                env[k] = v
        return env

    def argv(self, c, args, gflags=(), debug=False):
        a = [c.bin, *c.flags]
        if self.t.addressing and c.has(None, "--addressing-style"):
            a += ["--addressing-style", self.t.addressing]
        if self.t.ca_file and c.has(None, "--ca-certs-file"):
            a += ["--ca-certs-file", self.t.ca_file]
        if debug and c.kind == "s5cmd":
            a += ["--log", "debug"]
        return a + list(gflags) + list(args)

    def run(self, c, args, label, expect=0, gflags=(), env=None, stdin=None, cwd=None, timeout=1800,
            debug=False, watch=None, stop_on_watch=False, est=None, quiet=False, stdout=None):
        """Run one client command under `launch`. expect: an exit code, a tuple of them, "fail"
        (any non-zero) or None (anything; recorded only). stdout: a file for the command's output
        instead of capturing it (a large `cat`)."""
        load_wait, load1 = self.hold()
        argv = self.argv(c, args, gflags, debug)
        e = self.client_env(c, env)
        if debug and c.kind == "rs5cmd":
            e.setdefault("RUST_LOG", RETRY_ENV)
        base = os.path.join(self.live, f"{next(self.seq):06d}")
        js = base + ".json"
        cmd = [LAUNCH, "-c", CPUS, "-o", js, "-t", str(timeout), "--", *argv]
        t0 = time.time()
        tx0, rx0 = link_bytes()
        samples, worst, hit, pos, last = [(t0, tx0, rx0)], [0.0, 0.0], None, {".out": 0, ".err": 0}, t0
        fin = open(stdin, "rb") if stdin else subprocess.DEVNULL
        with open(stdout or base + ".out", "wb") as fo, open(base + ".err", "wb") as fe:
            p = subprocess.Popen(cmd, stdin=fin, stdout=fo, stderr=fe, env=e, cwd=cwd, start_new_session=True)
            try:
                while p.poll() is None:
                    time.sleep(0.1 if time.time() - t0 < 2 else 0.25)
                    now = time.time()
                    if now - last >= 2:
                        last = now
                        self.guard()
                        samples.append((now, *link_bytes()))
                        w = self.rate_check(samples)
                        worst = [max(worst[0], w[0]), max(worst[1], w[1])]
                        del samples[:-40]
                    for ext in ((".err",) if stdout else (".err", ".out")) if watch and hit is None else ():
                        with open(base + ext, "rb") as f:   # s5cmd logs debug lines to stdout, rs5cmd to stderr
                            f.seek(pos[ext])
                            chunk = f.read()
                        nl = chunk.rfind(b"\n")
                        if nl >= 0:
                            pos[ext] += nl + 1
                            m = watch.search(chunk[:nl].decode("utf-8", "replace"))
                            if m:
                                hit = m.group(0)
                                if stop_on_watch:
                                    self.stop_child(p.pid)
                                break
            except BaseException:
                try:
                    os.killpg(p.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                p.wait()
                raise
            finally:
                if stdin:
                    fin.close()
        tx1, rx1 = link_bytes()
        j = {}
        try:
            with open(js) as f:
                j = json.loads(f.read().strip().splitlines()[-1])
        except (OSError, ValueError, IndexError):
            pass
        rc = j.get("exit", p.returncode)
        if j.get("signal"):
            rc = -j["signal"]
        out = ""
        if not stdout:
            with open(base + ".out", "rb") as f:
                out = f.read().decode("utf-8", "replace")
        with open(base + ".err", "rb") as f:
            err = ANSI_RE.sub("", f.read().decode("utf-8", "replace"))
        retries = throttled = 0
        if debug:   # count the retry lines, keep a few, and take the debug lines out of both streams
            kept = []
            sifted = []
            for text in (out, err):
                keep = []
                for ln in text.splitlines():
                    if RETRY_RE[c.kind].search(ln):
                        retries += 1
                        throttled += bool(THROTTLE_RE.search(ln))
                        if retries <= 20:
                            kept.append(ln)
                    elif not DEBUG_RE.search(ln):
                        keep.append(ln)
                sifted.append("\n".join(keep) + ("\n" if keep else ""))
            out, err = sifted[0], sifted[1] + "".join(x + "\n" for x in kept)
        for n in (".err", ".json") if stdout else (".out", ".err", ".json"):
            os.unlink(base + n)
        ok = (expect is None or (expect == "fail" and rc != 0) or
              (isinstance(expect, tuple) and rc in expect) or rc == expect) and not j.get("timed_out")
        res = Res(client=c, label=label, argv=argv, rc=rc, out=out, err=err, ok=ok, hit=hit,
                  wall=j.get("wall_s", time.time() - t0), user=j.get("user_s", 0.0), sys=j.get("sys_s", 0.0),
                  rss=j.get("maxrss_kb", 0), tx=tx1 - tx0, rx=rx1 - rx0, retries=retries, throttled=throttled,
                  timed_out=bool(j.get("timed_out")))
        self.write("steps", {"t": "step", "label": label, "client": c.name, "argv": argv, "rc": rc,
                             "expect": expect, "ok": ok, "wall_s": round(res.wall, 3), "user_s": res.user,
                             "sys_s": res.sys, "maxrss_kb": res.rss, "tx_bytes": res.tx, "rx_bytes": res.rx,
                             "max_tx_bps": round(worst[0]), "max_rx_bps": round(worst[1]),
                             "avg_tx_bps": round(res.tx / res.wall) if res.wall >= 2 else 0,
                             "avg_rx_bps": round(res.rx / res.wall) if res.wall >= 2 else 0,
                             "load_wait_s": load_wait, "load1": load1, "timed_out": res.timed_out,
                             "out_lines": out.count("\n"), "err_lines": err.count("\n"),
                             "retries": retries if debug else None, "throttled": throttled if debug else None,
                             "watch_hit": hit, "est": est})
        self.write("outputs", {"label": label, "client": c.name, "stdout": cap_text(out), "stderr": cap_text(err)})
        cs = self.stats[self.scn]["clients"][c.name]
        cs["steps"] += 1
        cs["cpu_s"] += res.user + res.sys
        cs["wall_s"] += res.wall
        cs["maxrss_kb"] = max(cs["maxrss_kb"], res.rss)
        cs["tx"] += res.tx
        cs["rx"] += res.rx
        cs["retries"] += retries
        for k, v in (est or {}).items():
            cs[k] = cs.get(k, 0) + v
        if not quiet:
            self.say(f"  {self.scn} {c.name:<14} {label:<34} rc={rc} {res.wall:7.2f}s cpu={res.user + res.sys:6.2f}s "
                     f"rss={res.rss // 1024}MiB up={res.tx / MiB:.1f} down={res.rx / MiB:.1f} MiB"
                     + ("" if ok else "   <-- unexpected"))
        if not ok:
            cs["failed"] += 1
            self.finding("bug", f"{label}: exit {rc}, expected {expect}" + (" (timed out)" if res.timed_out else ""),
                         {"stderr": cap_text(err, 1500, 500), "stdout": cap_text(out, 600, 200)}, c.name, argv)
        self.budget_check()
        return res

    def stop_child(self, launch_pid):
        """SIGTERM the client (the child of `launch`), so that `launch` still reports."""
        try:
            with open(f"/proc/{launch_pid}/task/{launch_pid}/children") as f:
                for pid in f.read().split():
                    os.kill(int(pid), signal.SIGTERM)
        except (OSError, ValueError):
            os.killpg(launch_pid, signal.SIGTERM)

    def each(self, label, fn, compare=True, clients=None, **kw):
        """Run fn(client) -> args (or None to skip that client) for every client; diff the results."""
        results = []
        for c in clients or self.clients:
            args = fn(c)
            if args is None:
                continue
            results.append(self.run(c, args, label, **kw))
        if compare:
            self.compare(label, results)
        self.twin_time(label, results)
        return results

    def twin_time(self, label, results):
        """The lean path has no upload stall check (the SDK fails an attempt after 5 s without
        progress), so a lean command that takes far longer than its sdk twin is worth a look."""
        base = next((x for x in results if x.client.path == "sdk"), None)
        for x in results:
            if base and x.client.twin(base.client) and x.wall > 2 * base.wall + 5:
                self.finding("path", f"{label}: {x.client.name} took {x.wall:.1f} s, its twin {base.client.name} "
                             f"{base.wall:.1f} s (a stalled transfer?)", {"exit": x.rc, "stderr": x.err.strip()[-300:]},
                             x.client.name, x.argv)

    # --- comparing clients ---
    TS_RES = [(re.compile(r"\d{4}[/-]\d\d[/-]\d\d[ T]\d\d:\d\d:\d\d(\.\d+)?Z?"), "<ts>"),
              (re.compile(r"(request id|host id|RequestId|HostId|x-amz-request-id|x-amz-id-2)[:=] ?[^\s,)\"]+"), r"\1: <id>"),
              (re.compile(r',? ?\b(aws_request_id|s3_extended_request_id): "[^"]*"'), ""),
              (re.compile(r"\b[0-9a-f]{32}(-\d+)?\b"), "<etag>")]

    def norm(self, text, c):
        text = text.replace(f"{self.base}/{c.name}/", "<base>/").replace(f"{self.scratch}/{c.name}/", "<work>/")
        text = self.redact(text)
        for rx, sub in self.TS_RES:
            text = rx.sub(sub, text)
        return sorted(ln.rstrip() for ln in text.splitlines() if ln.strip())

    def compare(self, label, results, streams=("out", "err")):
        """Record where clients that should behave the same do not (a finding, not a failure)."""
        if len(results) < 2:
            return
        ref = results[0]
        for other in results[1:]:
            diffs = {}
            if ref.rc != other.rc:
                diffs["exit"] = {ref.client.name: ref.rc, other.client.name: other.rc}
            for name in streams:
                a, b = self.norm(getattr(ref, name), ref.client), self.norm(getattr(other, name), other.client)
                if a != b:
                    sa, sb = set(a), set(b)
                    diffs["std" + name] = {
                        "lines": {ref.client.name: len(a), other.client.name: len(b)},
                        f"only_{ref.client.name}": [cap_text(x, 300, 0) for x in sorted(sa - sb)[:6]],
                        f"only_{other.client.name}": [cap_text(x, 300, 0) for x in sorted(sb - sa)[:6]]}
            if diffs and other.client.twin(ref.client):   # one binary, two data paths: must be alike
                self.finding("path", f"{label}: {other.client.name} differs from {ref.client.name} in "
                             + ", ".join(diffs), diffs, argv=other.argv)
            elif diffs:
                self.finding("diff", f"{label}: {ref.client.name} and {other.client.name} differ in "
                             + ", ".join(diffs), diffs, argv=ref.argv)

    # --- checks ---
    def check(self, label, cond, detail="", c=None):
        cond = bool(cond)
        self.write("steps", {"t": "check", "label": label, "client": c.name if c else None, "ok": cond,
                             "detail": "" if cond else str(detail)})
        st = self.stats[self.scn]
        tgt = st["clients"][c.name] if c else st
        tgt["checks"] += 1
        if not cond:
            tgt["failed"] += 1
            self.say(f"   CHECK FAILED {c.name if c else ''} {label}: {detail}")
        return cond

    def info(self, path):
        if path not in self.manifest:
            self.manifest[path] = file_hashes(path)
        return self.manifest[path]

    def gen(self, name, size):
        path = self.src(name)
        if path not in self.manifest or self.manifest[path]["size"] != size:
            self.manifest[path] = gen_file(path, size, f"{self.id}/{name}")
        return path

    def expected_etag(self, path, part_size):
        k = (path, part_size)
        if k not in self.etag_cache:
            self.etag_cache[k] = multipart_etag(path, part_size)
        return self.etag_cache[k]

    def verify_upload(self, c, key, path, label, bucket=None, etag=True):
        """The object exists with the local file's size and (unless etag=False) its MD5-based ETag:
        the plain MD5 for a single PUT, the MD5 of the part MD5s for a multipart upload, with the
        part size read from the object itself (HEAD partNumber=1)."""
        bucket = bucket or self.t.bucket
        try:
            h = self.s3.head_object(bucket, key)
        except S3Error as e:
            return self.check(f"{label}: uploaded object exists", False, e, c)
        info = self.info(path)
        ok = self.check(f"{label}: size", int(h.get("content-length", -1)) == info["size"],
                        f"remote {h.get('content-length')}, local {info['size']}", c)
        if etag:
            et = h.get("etag", "").strip('"')
            if "-" in et:
                p1 = int(self.s3.head_object(bucket, key, part_number=1)["content-length"])
                want = self.expected_etag(path, p1)
            else:
                want = info["md5"]
            ok = self.check(f"{label}: etag", et == want, f"remote {et}, expected {want}", c) and ok
        return ok

    def verify_download(self, c, got, src, label):
        if not os.path.isfile(got):
            return self.check(f"{label}: downloaded file exists", False, "missing", c)
        a, b = file_hashes(got), self.info(src)
        return self.check(f"{label}: sha256", a["sha256"] == b["sha256"],
                          f"got {a['size']} bytes {a['sha256'][:16]}, source {b['size']} bytes {b['sha256'][:16]}", c)

    def remote_keys(self, prefix, bucket=None):
        return {o["key"]: o for o in self.s3.list_objects(bucket or self.t.bucket, prefix)}

    # --- scenarios ---
    def scenario(self, sid, fn):
        self.scn = sid
        self.clients = [c for c in self.all_clients if (c.only is None or sid in c.only)
                        and (c.name not in self.excluded or sid == "S11")]
        tx0, rx0 = link_bytes()
        st = self.stats[sid] = {
            "status": "ok", "tx0": tx0, "rx0": rx0, "checks": 0, "failed": 0, "t0": time.time(),
            "clients": {c.name: {"steps": 0, "checks": 0, "failed": 0, "cpu_s": 0.0, "wall_s": 0.0,
                                 "maxrss_kb": 0, "tx": 0, "rx": 0, "retries": 0, "note": ""}
                        for c in self.clients}}
        self.say(f"== {sid} {fn.__doc__.strip().splitlines()[0] if fn.__doc__ else ''}")
        try:
            fn(self)
        except Skip as e:
            st["status"] = f"skipped: {e}"
        except OverBudget as e:
            st["status"] = f"stopped: {e}"
            raise
        except Abort as e:
            st["status"] = f"aborted: {e}"
            raise
        except Exception as e:  # a harness or checker error: record it, go on with the next scenario
            st["status"] = f"error: {type(e).__name__}: {e}"
            self.event("scenario_error", trace=traceback.format_exc()[-3000:])
            self.say(f"   ERROR in {sid}: {type(e).__name__}: {e}")
        finally:
            tx1, rx1 = link_bytes()
            st["tx"], st["rx"], st["seconds"] = tx1 - tx0, rx1 - rx0, round(time.time() - st["t0"], 1)
            self.write("steps", {"t": "scenario", "status": st["status"], "tx_bytes": st["tx"], "rx_bytes": st["rx"],
                                 "seconds": st["seconds"], "checks": st["checks"], "failed": st["failed"],
                                 "clients": st["clients"], "requests_checker": self.s3.requests})
            self.say(f"== {sid} {st['status']}: {st['seconds']} s, {st['tx'] / MiB:.1f} MiB up, {st['rx'] / MiB:.1f} MiB down")
            self.scn = "between"
            self.clients = self.all_clients
            shutil.rmtree(os.path.join(self.scratch, "tmp"), ignore_errors=True)

    # --- cleanup ---
    def cleanup(self, local=True):
        """Delete everything this run created; list what is left."""
        self.scn = "cleanup"
        s3, b, rep = self.s3, self.t.bucket, {"deleted": 0, "left": [], "multipart": [], "notes": []}

        def purge(bucket, prefix, versions=False):
            if versions:
                items = [(v["key"], v["version_id"]) for v in s3.list_versions(bucket, prefix)]
            else:
                items = [o["key"] for o in s3.list_objects(bucket, prefix)]
            for i in range(0, len(items), 1000):
                for key, code in s3.delete_objects(bucket, items[i:i + 1000]):
                    rep["notes"].append(f"delete of {key} in {bucket}: {code}")
            rep["deleted"] += len(items)

        for suffix in list(self.state["scratch_buckets"]):
            name = b + suffix
            try:
                status = s3.head_bucket(name)
                if status == 200:
                    try:
                        purge(name, "", versions=True)
                    except S3Error:
                        purge(name, "")
                    s3.delete_bucket(name)
                    status = s3.head_bucket(name)
                if status == 200:
                    rep["left"].append(f"bucket {name}")
                elif status != 404:
                    rep["notes"].append(f"bucket {name}: HEAD returned {status}")
            except S3Error as e:
                rep["left"].append(f"bucket {name} ({e})")
        try:
            purge(b, self.base + "/")
            for key in self.state["extra_keys"]:
                try:
                    s3.delete_object(b, key)
                except S3Error as e:
                    rep["notes"].append(f"delete of {key}: {e}")
            left = [o["key"] for o in s3.list_objects(b, self.base + "/")]
            left += [k for k in self.state["extra_keys"] if s3.exists(b, k)]
            rep["left"] += [f"object {k}" for k in left[:50]]
            if len(left) > 50:
                rep["left"].append(f"... and {len(left) - 50} more objects")
        except S3Error as e:
            rep["notes"].append(f"object cleanup failed: {e}")
            rep["left"].append("unknown: the cleanup itself failed")
        try:
            ups = [u for u in s3.list_multipart_uploads(b, "")
                   if u["key"].startswith(self.base + "/") or u["key"] in self.state["extra_keys"]]
            for u in ups:
                s3.abort_multipart_upload(b, u["key"], u["upload_id"])
            rep["multipart"] = [f"{u['key']} (initiated {u['initiated']}): aborted" for u in ups]
            still = [u for u in s3.list_multipart_uploads(b, self.base + "/")]
            rep["left"] += [f"incomplete multipart upload {u['key']}" for u in still]
        except S3Error as e:
            rep["notes"].append(f"incomplete multipart uploads could not be listed ({e.status} {e.code}): "
                                "the policy needs s3:ListBucketMultipartUploads; the bucket's lifecycle rule "
                                "aborts them after one day")
        if local:
            shutil.rmtree(self.scratch, ignore_errors=True)
            shutil.rmtree(f"{PRIV}/home", ignore_errors=True)
            shutil.rmtree(f"{PRIV}/aws", ignore_errors=True)
            if os.path.exists(self.scratch):
                rep["left"].append("local scratch directory")
        self.write("steps", {"t": "cleanup", **rep})
        self.say(f"== cleanup: {rep['deleted']} objects deleted, {len(rep['multipart'])} multipart uploads aborted, "
                 f"left behind: {rep['left'] or 'nothing'}" + (f"; notes: {rep['notes']}" if rep["notes"] else ""))
        return rep


# --- report ----------------------------------------------------------------------------------

AWS_PRICE = {"put_per_1000": 0.005, "get_per_1000": 0.0004, "egress_per_gb": 0.09}


def read_jsonl(path):
    import gzip
    out = []
    for p, op in ((path, open), (path + ".gz", gzip.open)):
        if os.path.exists(p):
            with op(p, "rt", encoding="utf-8") as f:
                out += [json.loads(ln) for ln in f if ln.strip()]
    return out


def report(run_id):
    d = os.path.join(RESULTS, run_id)
    steps = read_jsonl(os.path.join(d, "steps.jsonl"))
    finds = read_jsonl(os.path.join(d, "findings.jsonl"))
    with open(os.path.join(d, "meta.json")) as f:
        meta = json.load(f)
    names = [c["name"] for c in meta["clients"]]
    try:
        import scenarios
    except Exception:   # the report still works without the registry of known issues
        scenarios = None
    L = [f"# Soak run {run_id}", "",
         f"Target: {meta['target_kind']} ({meta['target']}), scale `{meta['scale']}`"
         + (f", server version {meta['server_version']}" if meta.get("server_version") else "") + ".", ""]
    L += ["| Client | Binary | sha256 | Version | Flags | Env | Scenarios |", "|---|---|---|---|---|---|---|"]
    for c in meta["clients"]:
        L.append(f"| {c['name']} | {c['bin']} | {c['sha256'][:16]} | {c['version']} | "
                 f"{' '.join(c['flags']) or '-'} | {' '.join(c['env']) or '-'} | "
                 f"{', '.join(c.get('only') or []) or 'all'}{' (uploads only)' if c.get('uploads_only') else ''} |")
    source = meta.get("rs5cmd_source") or (meta.get("stage_b") and "perf/stage-b " + meta["stage_b"])
    if source:
        L += ["", f"rs5cmd binary: `{meta.get('rs5cmd_bin', 'b-stage')}`; its worktree was at {source.strip()} "
                  "when the binary was copied."]
    scen = [s for s in steps if s.get("t") == "scenario"]
    L += ["", "## Scenarios", "",
          "| Scenario | Ran | " + " | ".join(names)
          + " | Shared checks failed | MiB up | MiB down | Seconds |",
          "|---|---|" + "---|" * len(names) + "---|---|---|---|"]
    tot = {"tx": 0, "rx": 0, "put": 0, "get": 0}
    for s in scen:
        cells = []
        for n in names:
            c = s["clients"].get(n, {})
            verdict = "-" if not c.get("steps") else "pass" if not c.get("failed") else f"FAIL ({c['failed']})"
            cells.append(f"{verdict}: {c.get('steps', 0)} steps, {c.get('checks', 0)} checks"
                         + (f"; {c['note']}" if c.get("note") else ""))
            tot["put"] += c.get("put", 0)
            tot["get"] += c.get("get", 0)
        tot["tx"] += s["tx_bytes"]
        tot["rx"] += s["rx_bytes"]
        L.append(f"| {s['scn']} | {'to the end' if s['status'] == 'ok' else s['status']} | " + " | ".join(cells) + f" | {s['failed']} | "
                 f"{s['tx_bytes'] / MiB:.1f} | {s['rx_bytes'] / MiB:.1f} | {s['seconds']} |")
    L += ["", "## Resources per client", "",
          "| Scenario | Client | Wall s | CPU s | Max RSS MiB | MiB up | MiB down | Retries seen |", "|---|---|---|---|---|---|---|---|"]
    for s in scen:
        for n in names:
            c = s["clients"].get(n)
            if c and c["steps"]:
                L.append(f"| {s['scn']} | {n} | {c['wall_s']:.1f} | {c['cpu_s']:.1f} | {c['maxrss_kb'] / 1024:.0f} | "
                         f"{c['tx'] / MiB:.1f} | {c['rx'] / MiB:.1f} | {c['retries']} |")
    rows = {}
    for s in steps:
        if s.get("t") == "step" and s["scn"] == "S14" and s["label"].startswith("W"):
            rows.setdefault((re.sub(r" r\d+$", "", s["label"]), s["client"]), []).append(s)
    if rows:
        sdk = next((c["name"] for c in meta["clients"] if c.get("path") == "sdk"), None)
        s5 = next((c["name"] for c in meta["clients"] if c["kind"] == "s5cmd"), None)
        cpu = {k: statistics.median(x["user_s"] + x["sys_s"] for x in ss) for k, ss in rows.items()}
        ratio = lambda row, n, ref: (f"{cpu[(row, n)] / cpu[(row, ref)]:.2f}"   # noqa: E731
                                     if ref and ref != n and cpu.get((row, ref)) else "-")
        L += ["", "## S14 performance rows (median of the rounds)", "",
              "Wall is bound by the rate limit; CPU is what compares. The criterion for the lean path is CPU at or "
              "below the sdk path on every row except `W7 pipe` (10 % and 0.05 s allowed for noise).", "",
              "| Row | Client | Rounds | Wall s | User s | Sys s | CPU s | CPU / sdk | CPU / s5cmd | Max RSS MiB |",
              "|---|---|---|---|---|---|---|---|---|---|"]
        for (row, n), ss in sorted(rows.items()):
            med = lambda k: statistics.median(x[k] for x in ss)  # noqa: E731
            L.append(f"| {row} | {n} | {len(ss)} | {med('wall_s'):.2f} | {med('user_s'):.2f} | {med('sys_s'):.2f} | "
                     f"{cpu[(row, n)]:.2f} | {ratio(row, n, sdk)} | {ratio(row, n, s5)} | "
                     f"{max(x['maxrss_kb'] for x in ss) / 1024:.0f} |")
    gb_down = tot["rx"] / 1e9
    cost = (tot["put"] / 1000 * AWS_PRICE["put_per_1000"] + tot["get"] / 1000 * AWS_PRICE["get_per_1000"]
            + gb_down * AWS_PRICE["egress_per_gb"])
    L += ["", "## Volume", "",
          f"- Sent {tot['tx'] / 1e9:.2f} GB, received {gb_down:.2f} GB (the soak container's interface counters, "
          "checker traffic included).",
          f"- Requests by the clients, estimated from the operation counts: about {tot['put']:,} of the PUT / COPY / "
          f"POST / LIST class and {tot['get']:,} of the GET / HEAD class.",
          f"- At AWS list prices (us and eu regions) that is about ${cost:.2f}: requests "
          f"${tot['put'] / 1000 * AWS_PRICE['put_per_1000'] + tot['get'] / 1000 * AWS_PRICE['get_per_1000']:.2f}, "
          f"egress ${gb_down * AWS_PRICE['egress_per_gb']:.2f}."
          + ("" if meta["target_kind"] == "aws" else " (This target is not AWS: nothing was billed.)")]
    g = meta.get("guard", {})
    if g:
        L += ["", "## Guards", "",
              f"- Host conntrack count: highest seen {g.get('max_conntrack')} (warn above {CT_WARN:,}, abort above {CT_ABORT:,}).",
              f"- Load holds: {g.get('load_holds', 0)}, {g.get('load_hold_s', 0)} s in all.",
              f"- Highest rate (any window of 4 s or more, or a whole command of 2 s or more): {g.get('max_tx_bps', 0) / 1e6:.1f} MB/s up, "
              f"{g.get('max_rx_bps', 0) / 1e6:.1f} MB/s down (cap {CAP_BYTES / 1e6:.0f})."]
    cl = [s for s in steps if s.get("t") == "cleanup"]
    if cl:
        c = cl[-1]
        L += ["", "## Cleanup", "", f"- {c['deleted']} objects deleted; left behind: {', '.join(c['left']) or 'nothing'}."]
        L += [f"- Incomplete multipart upload: {m}" for m in c["multipart"]] + [f"- Note: {n}" for n in c["notes"]]
    bad, last = [], {}
    for s in steps:
        if s.get("t") == "step":
            last[(s["scn"], s["client"])] = s
            if not s["ok"]:
                bad.append((s, f"exit {s['rc']}, expected {s['expect']}" + (" (timed out)" if s.get("timed_out") else ""), s))
        elif s.get("t") == "check" and not s["ok"]:
            bad.append((s, s["detail"], last.get((s["scn"], s["client"]))))
    L += ["", f"## Failed steps and checks ({len(bad)})", "",
          "A client that fails where the reference (s5cmd) passes, wrong data, or an unexpected exit code. "
          "The command is the client's last one before the check."]
    for i, (s, detail, step) in enumerate(bad, 1):
        issue = scenarios.known_issue(s["scn"], s["label"]) if scenarios else None
        L.append(f"{i}. **{s['scn']} {s.get('client') or 'shared'}**: {s['label']}" + (f" (known: {issue})" if issue else ""))
        L.append("   - Detail: `" + str(detail).replace("`", "'")[:500] + "`")
        if step:
            L.append("   - Command: `" + " ".join(step["argv"]).replace("`", "'")[:400] + "`")
    if any(c.get("path") == "lean" for c in meta["clients"]):
        n_path = sum(1 for f in finds if f["kind"] == "path")
        L += ["", "## Data paths: lean against sdk", "",
              f"- `path` findings: {n_path}. Each is one command where the same binary behaved differently between "
              "its data paths (exit code or output), or where a lean command took far longer than its sdk twin. "
              "They are listed under Findings; there should be none.",
              "- Intended or known differences, checked or left out as described:"]
        L += [f"  {i}. {text}" for i, text in enumerate(scenarios.EXPECTED if scenarios else [], 1)]
    L += ["", f"## Findings ({len(finds)} recorded, grouped where they say the same about several steps)", ""]
    order = {"bug": 0, "path": 0.5, "endpoint": 1, "gap": 2, "diff": 3, "note": 4}
    groups = {}
    for f in sorted(finds, key=lambda f: (order.get(f["kind"], 9), int(re.sub(r"\D", "", f["scn"]) or 99))):
        what = f["title"].split(": ", 1)[-1] if f["kind"] in ("diff", "path") else f["title"]
        if f["kind"] == "bug":
            what = re.sub(r"^.*?: (exit .*)$", r"\1", f["title"]) + " " + json.dumps(f.get("detail") or "")[:160]
        sig = (f["kind"], f.get("client"), re.sub(r"\d+", "N", what)[:200],
               re.sub(r"\d+|<base>/\S+|<work>/\S+|<scratch>/\S+|s3://\S+", "N", json.dumps(f.get("detail") or "", ensure_ascii=False))[:240]
               if f["kind"] in ("diff", "path") else "")
        groups.setdefault(sig, []).append(f)
    for i, fs in enumerate(groups.values(), 1):
        f = fs[0]
        where = sorted({x["scn"] for x in fs}, key=lambda s: int(re.sub(r"\D", "", s) or 99))
        L.append(f"{i}. **[{f['kind']}] {', '.join(where)}{' ' + f['client'] if f.get('client') else ''}**: {f['title']}"
                 + (f" (and {len(fs) - 1} more steps: " + "; ".join(sorted({x['title'].split(':')[0] for x in fs[1:]}))[:300] + ")"
                    if len(fs) > 1 else ""))
        if f.get("argv"):
            L.append("   - Command: `" + " ".join(f["argv"]).replace("`", "'")[:400] + "`")
        if f.get("detail"):
            L.append("   - Detail: `" + json.dumps(f["detail"], ensure_ascii=False).replace("`", "'")[:700] + "`")
    with open(os.path.join(d, "summary.md"), "w", encoding="utf-8") as f:
        f.write("\n".join(L) + "\n")
    return os.path.join(d, "summary.md")


# --- commands --------------------------------------------------------------------------------

def prepare():
    """Copy the measured binaries from the review's bins volume (read-only) into ours, and check
    each copy against that volume's manifest (a build may be writing while we copy)."""
    listed = {}
    try:
        with open(f"{REVIEW_BINS}/manifest.jsonl") as f:
            for ln in f:
                try:
                    d = json.loads(ln)
                    listed[d["name"]] = d.get("sha256")
                except (ValueError, KeyError):
                    pass
    except OSError:
        pass
    out = {}
    for name in (RS5CMD_BIN, "s5cmd", "launch"):
        src, dst = f"{REVIEW_BINS}/{name}", f"{BINS}/{name}"
        for attempt in range(4):
            shutil.copy2(src, dst + ".new")
            sha = file_hashes(dst + ".new")["sha256"]
            if listed.get(name) in (None, sha) and sha == file_hashes(src)["sha256"]:
                break
            time.sleep(10)
        else:
            raise SystemExit(f"soak: {name} in the review's bins volume does not match its manifest")
        os.replace(dst + ".new", dst)
        st = os.stat(dst)
        out[name] = {"sha256": sha, "size": st.st_size, "in_manifest": name in listed,
                     "mtime": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(st.st_mtime))}
        print(f"prepared {name}: {st.st_size} bytes, sha256 {sha[:16]}, built {out[name]['mtime']}")
    with open(f"{BINS}/soak-bins.json", "w") as f:
        json.dump(out, f)


def make_clients(a):
    clients = default_clients()
    for spec in a.client_bin:
        name, path = spec.split("=", 1)
        clients[name] = Client(name, path)
    for spec in a.client_flag:
        name, flag = spec.split("=", 1)
        clients[name].flags.append(flag)
    for spec in a.client_env:
        name, kv = spec.split("=", 1)
        k, v = kv.split("=", 1)
        clients[name].env[k] = v
    for spec in a.client_only:
        name, ids = spec.split("=", 1)
        clients[name].only = {s for s in ids.split(",") if s} or None
    names = a.clients.split(",") if a.clients else list(clients)
    return [clients[n] for n in names]


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("prepare")
    r = sub.add_parser("run")
    r.add_argument("--target", required=True)
    r.add_argument("--scale", default="dry")
    r.add_argument("--only", default="")
    r.add_argument("--skip", default="")
    r.add_argument("--run-id")
    r.add_argument("--keep", action="store_true", help="skip the cleanup at the end")
    r.add_argument("--clients", default="", help="names to run, in order (default: all)")
    r.add_argument("--client-bin", action="append", default=[], metavar="NAME=PATH")
    r.add_argument("--client-flag", action="append", default=[], metavar="NAME=FLAG")
    r.add_argument("--client-env", action="append", default=[], metavar="NAME=KEY=VALUE")
    r.add_argument("--client-only", action="append", default=[], metavar="NAME=S1,S11",
                   help="the scenarios a client takes part in (empty: all)")
    r.add_argument("--set", action="append", default=[], metavar="KEY=VALUE", help="override a scale value")
    c = sub.add_parser("cleanup")
    c.add_argument("--target", required=True)
    c.add_argument("--run-id", required=True)
    p = sub.add_parser("report")
    p.add_argument("--run-id", required=True)
    a = ap.parse_args()

    if a.cmd == "prepare":
        return prepare()
    if a.cmd == "report":
        print(report(a.run_id))
        return
    import scenarios
    target = Target(a.target)
    if a.cmd == "cleanup":
        if not os.path.isdir(os.path.join(RESULTS, a.run_id)):
            raise SystemExit(f"soak: no results directory for run {a.run_id}")
        run = Run(target, list(default_clients().values()), scenarios.SCALES["dry"], a.run_id, "dry")
        rep = run.cleanup()
        for f in run.files.values():
            f.close()
        if os.path.exists(os.path.join(run.dir, "meta.json")):
            print(report(a.run_id))
        sys.exit(1 if rep["left"] else 0)

    scale = dict(scenarios.SCALES[a.scale])
    for kv in a.set:
        k, v = kv.split("=", 1)
        scale[k] = type(scale[k])(v) if not isinstance(scale[k], (dict, list, tuple)) else json.loads(v)
    clients = make_clients(a)
    run_id = a.run_id or f"{a.target}-{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}"
    run = Run(target, clients, scale, run_id, a.scale)
    bins = {}
    if os.path.exists(f"{BINS}/soak-bins.json"):
        with open(f"{BINS}/soak-bins.json") as f:
            bins = json.load(f)
    meta = {"run_id": run_id, "target": a.target, "target_kind": target.kind, "scale": a.scale,
            "region_set": bool(target.region), "addressing": target.addressing, "server_version": target.version,
            "rs5cmd_bin": RS5CMD_BIN, "rs5cmd_source": os.environ.get("SOAK_RS5CMD_SRC"),
            "image": os.environ.get("SOAK_IMAGE_ID"),
            "cpus": CPUS, "cap_bytes_per_s": CAP_BYTES, "load_max": LOAD_MAX, "bins": bins,
            "clients": [{"name": c.name, "bin": c.bin, "kind": c.kind, "sha256": c.sha256(), "version": c.version(),
                         "flags": c.flags, "env": sorted(c.env), "path": c.path, "trailers": c.trailers,
                         "only": sorted(c.only) if c.only else None, "uploads_only": c.uploads_only} for c in clients],
            "started": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}

    def save_meta():
        with open(os.path.join(run.dir, "meta.json"), "w") as f:
            f.write(run.redact(json.dumps(meta, indent=1)))

    save_meta()
    run.say(f"soak run {run_id}: target {a.target} ({target.kind}), scale {a.scale}, clients "
            + ", ".join(f"{c.name}={os.path.basename(c.bin)}" for c in clients))
    only = [s for s in a.only.split(",") if s]
    skip = [s for s in a.skip.split(",") if s]
    outcome = "finished"
    if target.addressing == "virtual" and any(not c.has(None, "--addressing-style") for c in clients):
        run.finding("note", "virtual-hosted addressing was asked for, but "
                    + ", ".join(c.name for c in clients if not c.has(None, "--addressing-style"))
                    + " has no flag for it on a custom endpoint and uses path style")
    try:
        run.guard()
        scenarios.endpoint_probe(run)
        for sid, fn in scenarios.SCENARIOS:
            if (only and sid not in only) or sid in skip:
                continue
            run.scenario(sid, fn)
    except (Abort, OverBudget) as e:
        outcome = f"{type(e).__name__}: {e}"
        run.event("stopped", reason=outcome)
        run.say(f"!! soak stopped: {outcome}")
    except KeyboardInterrupt:
        outcome = "interrupted"
    rep = None
    if not a.keep:
        try:
            rep = run.cleanup()
        except Exception as e:  # never lose the results over a failed cleanup
            run.say(f"!! cleanup failed: {type(e).__name__}: {e}")
    events = read_jsonl(os.path.join(run.dir, "events.jsonl"))
    steps = read_jsonl(os.path.join(run.dir, "steps.jsonl"))
    holds = [e for e in events if e.get("event") == "load_hold"]
    meta.update(outcome=outcome, finished=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                seconds=round(time.time() - run.t_start),
                guard={"max_conntrack": run.max_conntrack, "load_holds": len(holds),
                       "load_hold_s": round(sum(e["wait_s"] for e in holds)),
                       "max_tx_bps": max([max(s.get("max_tx_bps", 0), s.get("avg_tx_bps", 0)) for s in steps if s.get("t") == "step"] or [0]),
                       "max_rx_bps": max([max(s.get("max_rx_bps", 0), s.get("avg_rx_bps", 0)) for s in steps if s.get("t") == "step"] or [0])})
    save_meta()
    for f in run.files.values():
        f.close()
    print(report(run_id))
    uid, gid = int(os.environ.get("HOST_UID", "0")), int(os.environ.get("HOST_GID", "0"))
    if uid:
        for root, dirs, fs in os.walk(run.dir):
            for n in [root] + [os.path.join(root, x) for x in fs]:
                os.chown(n, uid, gid)
    failed = any(s.get("failed") or any(c.get("failed") for c in s["clients"].values()) or
                 s["status"].startswith(("error", "stopped", "aborted"))
                 for s in steps if s.get("t") == "scenario")
    sys.exit(0 if outcome == "finished" and not failed and not (rep and rep["left"]) else 1)


if __name__ == "__main__":
    main()
